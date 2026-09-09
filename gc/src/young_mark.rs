// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Parallel young-generation marking support.
//!
//! The default young collector (`GenerationalHeap::sweep_young_non_moving`)
//! is NON-MOVING: nothing is relocated, and liveness is recorded exclusively
//! in a per-cycle SIDE channel — the collector never writes `GC_FLAG_MARKED`
//! through a conservative candidate (see the `side_marks` rationale in
//! `gen_heap.rs`). That makes the mark phase a pure, read-only transitive
//! closure over a frozen heap: every mutator is stopped at a safepoint, the
//! only mutable state is the side mark set, and the closure is confluent —
//! the final marked set does not depend on traversal order.
//!
//! This module supplies the two pieces that turn that observation into a
//! parallel mark:
//!
//! * [`YoungMarkBits`] — a lock-free replacement for the `FxHashSet<usize>`
//!   side mark set. One bit per 8 bytes of young from-space, claimed with a
//!   single `fetch_or`. `try_mark` is an atomic test-and-set, so exactly one
//!   thread ever claims (and therefore ever scans) a given object. It also
//!   removes the `O(n log n)` sort that built `side_sorted`: iterating the
//!   bitmap yields addresses in ascending order by construction.
//! * [`drain_parallel`] — a work-sharing drain of the mark worklist across
//!   scoped threads, with a global batch stack and per-worker local stacks.
//!
//! Plus [`zero_spans_parallel`], which memsets the sweep's reclaimed spans on
//! the same worker count. The spans are disjoint and already computed, so
//! that one needs no synchronisation at all.
//!
//! # Why this is race-free
//!
//! * Object bodies are only ever READ during marking (`for_each_ref_slot`).
//! * The only write is `YoungMarkBits::try_mark`, an atomic RMW.
//! * An object is pushed onto a worklist only by the thread whose `try_mark`
//!   observed the bit as clear, so each object is scanned at most once.
//! * `std::thread::scope` joins every worker before the caller reads the
//!   bitmap, giving the happens-before edge for the final `collect_marked`.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

/// One bit per 8 bytes of a young from-space region.
///
/// Addresses outside `[base, base + span)` are silently ignored (callers
/// already screen with `in_young`; this is belt-and-braces).
pub(crate) struct YoungMarkBits {
    words: *mut AtomicU64,
    nwords: usize,
    base: usize,
    span: usize,
}

// SAFETY: every access to `words` goes through atomic operations on
// `AtomicU64`; the allocation is owned exclusively by this value and freed
// once in `Drop`.
unsafe impl Send for YoungMarkBits {}
// SAFETY: as above — all shared access is atomic.
unsafe impl Sync for YoungMarkBits {}

impl YoungMarkBits {
    /// Cover `[base, base + span)`. The backing store is `alloc_zeroed`, so a
    /// multi-megabyte bitmap costs a zero-page mapping rather than a memset.
    pub(crate) fn new(base: usize, span: usize) -> Self {
        let nbits = span.div_ceil(8);
        let nwords = nbits.div_ceil(64);
        if nwords == 0 {
            return Self {
                words: std::ptr::NonNull::<AtomicU64>::dangling().as_ptr(),
                nwords: 0,
                base,
                span: 0,
            };
        }
        let layout = std::alloc::Layout::array::<AtomicU64>(nwords)
            .expect("young mark bitmap layout overflow");
        // SAFETY: `nwords > 0` so the layout is non-zero-sized. An all-zero
        // bit pattern is a valid `AtomicU64` (value 0).
        let words = unsafe { std::alloc::alloc_zeroed(layout) } as *mut AtomicU64;
        if words.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Self {
            words,
            nwords,
            base,
            span,
        }
    }

    #[inline]
    fn locate(&self, addr: usize) -> Option<(usize, u64)> {
        if addr < self.base {
            return None;
        }
        let off = addr - self.base;
        if off >= self.span {
            return None;
        }
        let bit = off >> 3;
        Some((bit >> 6, 1u64 << (bit & 63)))
    }

    /// Atomically claim `addr`. Returns `true` exactly once per address, for
    /// the thread that transitioned the bit from 0 to 1.
    #[inline]
    pub(crate) fn try_mark(&self, addr: usize) -> bool {
        match self.locate(addr) {
            None => false,
            Some((w, mask)) => {
                // SAFETY: `locate` bounds-checked `addr`, so `w < nwords`.
                let prev = unsafe { (*self.words.add(w)).fetch_or(mask, Ordering::Relaxed) };
                prev & mask == 0
            }
        }
    }

    /// Clear `addr`'s bit. Returns `true` if it had been set.
    ///
    /// The ONLY caller is the late base-resolution pass in
    /// `sweep_young_non_moving`, and only for an address that pass PROVED is
    /// object-INTERIOR by chaining the arena's own object grid. Such a mark
    /// retains nothing (the sweep matches marks against object STARTS) while
    /// still counting as a "live base" for the phantom-extent guard, which
    /// then condemns the perfectly valid object containing it. Removing it is
    /// therefore neutral for retention and restores the guard's premise --
    /// see `SWEEP_PHANTOM_INTERIOR_MARKS` in `gen_heap.rs`.
    #[inline]
    pub(crate) fn unmark(&self, addr: usize) -> bool {
        match self.locate(addr) {
            None => false,
            Some((w, mask)) => {
                // SAFETY: `locate` bounds-checked `addr`, so `w < nwords`.
                let prev = unsafe { (*self.words.add(w)).fetch_and(!mask, Ordering::Relaxed) };
                prev & mask != 0
            }
        }
    }

    /// Is `addr` marked? (Replacement for `side_marks.contains`.)
    #[inline]
    pub(crate) fn contains(&self, addr: usize) -> bool {
        match self.locate(addr) {
            None => false,
            Some((w, mask)) => {
                // SAFETY: `locate` bounds-checked `addr`, so `w < nwords`.
                let v = unsafe { (*self.words.add(w)).load(Ordering::Relaxed) };
                v & mask != 0
            }
        }
    }

    /// Every marked address, ASCENDING — the `side_sorted` view, free.
    pub(crate) fn collect_marked(&self) -> Vec<usize> {
        let mut out = Vec::new();
        for w in 0..self.nwords {
            // SAFETY: `w < nwords`.
            let mut word = unsafe { (*self.words.add(w)).load(Ordering::Relaxed) };
            while word != 0 {
                let b = word.trailing_zeros() as usize;
                word &= word - 1;
                out.push(self.base + ((w * 64 + b) << 3));
            }
        }
        out
    }
}

/// A word-batching cursor over [`ObjectStartBits`] -- see [`ObjectStartBits::run`].
///
/// Flushes on drop, so a walk that exits early (every refusal path in
/// `objstart_chunk` does) still publishes the bits it legitimately recorded.
/// That matters only for the sequential walk, whose caller keeps the bitmap; a
/// refused parallel chunk has its whole bitmap thrown away regardless.
pub(crate) struct StartRun<'a> {
    bits: &'a ObjectStartBits,
    word: usize,
    mask: u64,
}

impl StartRun<'_> {
    /// Record an object start. Same contract as [`ObjectStartBits::insert`]:
    /// `false` means the address is outside the span or not 8-byte aligned,
    /// and the caller must not run a moving cycle against this bitmap.
    #[inline]
    pub(crate) fn insert(&mut self, addr: usize) -> bool {
        match self.bits.locate(addr) {
            None => false,
            Some((w, mask)) => {
                if w != self.word {
                    self.flush();
                    self.word = w;
                }
                self.mask |= mask;
                true
            }
        }
    }

    #[inline]
    pub(crate) fn flush(&mut self) {
        if self.mask != 0 {
            self.bits.words[self.word].fetch_or(self.mask, Ordering::Relaxed);
            self.mask = 0;
        }
    }
}

impl Drop for StartRun<'_> {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Exact "is this address an object start in young from-space?" membership,
/// as one bit per 8 bytes of `[base, base + span)`.
///
/// The moving (Cheney) collector needs this predicate to reject aligned
/// interior words arriving from conservative roots before it forwards through
/// them. It used to be an `FxHashSet<usize>` populated by the pre-collection
/// from-space walk — one insert per object, live or dead. That made a moving
/// collection cost O(objects allocated) rather than O(objects surviving):
/// on bt18 at `-Xmx8g`, `HashMap::insert` plus `reserve_rehash` were 49% of
/// the entire process and one moving cycle cost ~6.1 s, against a 1.4 s
/// whole-program run on the default non-moving collector.
///
/// A bitmap is an exact substitute because every object start is 8-byte
/// aligned: `gen_object_total_size` rounds each footprint up to 8, the walk
/// begins at the (page-aligned) arena base, and the free-block / TLAB-tail
/// skips it honours are allocation-granular. [`Self::insert`] reports an
/// unaligned start rather than aliasing a neighbour's bit, and the caller
/// treats that as a walk that did not complete.
///
/// Shared across the object-start walk's workers (gc-genpause F1): the words
/// are atomic and `insert` is a `fetch_or`, so a chunked parallel walk and the
/// sequential fallback fill the same bitmap by the same rule. Every access is
/// `Relaxed` -- the workers are joined before any reader runs, and that join is
/// the happens-before edge; the bits carry no other data to order.
pub(crate) struct ObjectStartBits {
    words: Vec<AtomicU64>,
    base: usize,
    span: usize,
}

impl ObjectStartBits {
    /// Cover `[base, base + span)`. `vec![0u64; n]` allocates zeroed, so a
    /// multi-megabyte bitmap costs a zero-page mapping, not a memset.
    pub(crate) fn new(base: usize, span: usize) -> Self {
        let nwords = span.div_ceil(8).div_ceil(64);
        Self {
            words: (0..nwords).map(|_| AtomicU64::new(0)).collect(),
            base,
            span,
        }
    }

    #[inline]
    fn locate(&self, addr: usize) -> Option<(usize, u64)> {
        let off = addr.checked_sub(self.base)?;
        if off >= self.span || off & 7 != 0 {
            return None;
        }
        let bit = off >> 3;
        Some((bit >> 6, 1u64 << (bit & 63)))
    }

    /// Record an object start. Returns `false` when `addr` is outside the
    /// covered span or is not 8-byte aligned — either means the bitmap cannot
    /// represent this walk exactly, and the caller must not run a moving cycle
    /// against it.
    #[inline]
    pub(crate) fn insert(&self, addr: usize) -> bool {
        match self.locate(addr) {
            None => false,
            Some((w, mask)) => {
                self.words[w].fetch_or(mask, Ordering::Relaxed);
                true
            }
        }
    }

    /// `(base, span)` — the arena extent this bitmap can represent at all.
    ///
    /// Exposed for the evacuator's refusal census: an address the bitmap says
    /// is not an object start because it lies OUTSIDE the covered span is a
    /// completely different finding from one the walk simply did not visit,
    /// and the report has to be able to tell them apart.
    #[inline]
    pub(crate) fn extent(&self) -> (usize, usize) {
        (self.base, self.span)
    }

    /// Membership. An address outside the span, or not 8-byte aligned, is not
    /// an object start — the same answer the `FxHashSet` gave.
    #[inline]
    pub(crate) fn contains(&self, addr: usize) -> bool {
        match self.locate(addr) {
            None => false,
            Some((w, mask)) => self.words[w].load(Ordering::Relaxed) & mask != 0,
        }
    }

    /// Number of recorded starts. Diagnostics and tests only -- deliberately
    /// NOT maintained incrementally.
    ///
    /// # gc-genpause F1: this counter was the whole parallel regression
    ///
    /// It used to be an `AtomicUsize` bumped inside `insert`. That is one
    /// process-global cache line taking a `lock add` from every worker for
    /// every object in from-space -- ~6.7 M of them on a 268 MB space -- so the
    /// "parallel" walk was 443-522 ms against the sequential walk's 227-274 ms.
    /// A shared counter incremented per unit of work is the canonical way to
    /// make a parallel loop slower than the serial one, and it was maintained
    /// for a value nothing outside the unit tests reads.
    ///
    /// Popcounting on demand is O(words), runs only when someone asks, and
    /// gives the identical answer.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.words
            .iter()
            .map(|w| w.load(Ordering::Relaxed).count_ones() as usize)
            .sum()
    }

    /// Batches bit sets into ONE atomic OR per 512-byte word.
    ///
    /// A walk visits object starts in increasing address order, and one word
    /// of this bitmap covers 512 bytes of arena -- about a dozen objects at
    /// typical sizes. Setting each bit with its own `fetch_or` therefore pays
    /// a `lock or` roughly twelve times per word for writes that could be one.
    /// Accumulating the word and flushing it on the way past cuts the atomic
    /// count by that factor, and costs a compare and an OR per object.
    ///
    /// Correct for any order, not just ascending: a revisit of an earlier word
    /// simply flushes and re-acquires it.
    pub(crate) fn run(&self) -> StartRun<'_> {
        StartRun {
            bits: self,
            word: usize::MAX,
            mask: 0,
        }
    }
}

impl Drop for YoungMarkBits {
    fn drop(&mut self) {
        if self.nwords == 0 {
            return;
        }
        let layout = std::alloc::Layout::array::<AtomicU64>(self.nwords)
            .expect("young mark bitmap layout overflow");
        // SAFETY: `words` came from `alloc_zeroed` with this exact layout.
        unsafe { std::alloc::dealloc(self.words as *mut u8, layout) };
    }
}

// ---------------------------------------------------------------------------
// Worker-count policy
// ---------------------------------------------------------------------------

/// `CRATONVM_GC_PAR_THREADS`: explicit worker count for the young collector.
/// `0`/`1` disables parallelism entirely; `>= 2` forces that many workers
/// regardless of heap size (this is what the GC-stress matrix uses to
/// exercise the parallel path on a tiny heap). Unset = automatic.
#[inline]
fn configured_threads() -> Option<usize> {
    crate::gc_flags().gc_par_threads
}

/// Automatic worker count when `CRATONVM_GC_PAR_THREADS` is unset.
fn auto_threads() -> usize {
    static G: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(8)
    })
}

/// Young-gen bytes below which parallelism never pays for itself (thread
/// spawn + bitmap iteration dominate). Override with
/// `CRATONVM_GC_PAR_MIN_BYTES`.
#[inline]
fn min_parallel_bytes() -> usize {
    crate::gc_flags().gc_par_min_bytes
}

/// Workers to use for a collection whose from-space holds `used` bytes.
/// `1` means "run the sequential path" — the caller must still be correct
/// with a single worker, which is exactly how the code is structured.
pub(crate) fn young_gc_threads(used: usize) -> usize {
    match configured_threads() {
        // Explicit: honour it, including on a tiny heap (stress testing).
        Some(n) => n.max(1),
        None => {
            if used < min_parallel_bytes() {
                1
            } else {
                auto_threads()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Parallel worklist drain
// ---------------------------------------------------------------------------

struct DrainState {
    stack: Vec<usize>,
    idle: usize,
    done: bool,
}

// ---------------------------------------------------------------------------
// Drain engagement census
// ---------------------------------------------------------------------------

/// Calls to [`drain_parallel`], calls that actually spawned workers, the worker
/// count of the LAST call, and the total time inside the drain.
///
/// # Why this is not optional instrumentation
///
/// The `+153 %`-at-four-workers reading that started the parallel-marking
/// question is a worker-count A/B, and a worker-count A/B is a measurement of
/// the collector only if the lever engages. On G1 the same question turned out
/// to be TWO INERT LEVERS (`CRATONVM_GC_PAR_THREADS` and
/// `-XX:ParallelGCThreads` both left `workers_last` at 23) and one live one, so
/// every prior A/B on either of the first two compared a binary against itself
/// and produced a number about nothing. `workers_last` is the line that makes
/// that visible on the generational side before anyone reads a pause figure.
///
/// Always on: four relaxed counter updates per COLLECTION, not per object.
static DRAIN_CALLS: AtomicUsize = AtomicUsize::new(0);
static DRAIN_PARALLEL_CALLS: AtomicUsize = AtomicUsize::new(0);
static DRAIN_WORKERS_LAST: AtomicUsize = AtomicUsize::new(0);
static DRAIN_NANOS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `(calls, parallel_calls, workers_last, nanos)` -- see [`DRAIN_CALLS`].
pub fn drain_census() -> (usize, usize, usize, u64) {
    (
        DRAIN_CALLS.load(Ordering::Relaxed),
        DRAIN_PARALLEL_CALLS.load(Ordering::Relaxed),
        DRAIN_WORKERS_LAST.load(Ordering::Relaxed),
        DRAIN_NANOS.load(Ordering::Relaxed),
    )
}

/// Batch size handed to a worker per global-stack acquisition.
const ACQUIRE_CHUNK: usize = 256;
/// Local stack depth at which a worker publishes surplus work.
const SPILL_HIGH: usize = 2048;
/// Depth a worker keeps for itself when spilling.
const SPILL_KEEP: usize = 512;

/// Drain `seed` with `threads` workers, calling `scan(addr, &mut worklist)`
/// for each address. `scan` pushes newly-claimed addresses onto the worklist
/// it is handed.
///
/// With `threads <= 1` this is a plain sequential loop on the calling thread
/// (no threads are spawned), so the single-worker path is identical in
/// behaviour to the pre-parallel collector.
pub(crate) fn drain_parallel<F>(seed: Vec<usize>, threads: usize, scan: F)
where
    F: Fn(usize, &mut Vec<usize>) + Sync,
{
    // ENGAGEMENT, before anything else. The standing question about this
    // function is a worker-count A/B ("four workers cost +153% pause against
    // zero"), and a worker-count A/B is only a measurement of the collector if
    // the lever reaches the collector. The sibling question on G1 turned out
    // to be two INERT levers and one live one, so every prior A/B on the first
    // two had compared a binary against itself. Recording what this call
    // actually ran with is what makes that mistake visible instead of silent.
    DRAIN_CALLS.fetch_add(1, Ordering::Relaxed);
    DRAIN_WORKERS_LAST.store(threads, Ordering::Relaxed);
    if threads > 1 {
        DRAIN_PARALLEL_CALLS.fetch_add(1, Ordering::Relaxed);
    }
    let __t0 = std::time::Instant::now();
    let __done = DrainTimer(__t0);
    struct DrainTimer(std::time::Instant);
    impl Drop for DrainTimer {
        fn drop(&mut self) {
            DRAIN_NANOS.fetch_add(self.0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
    }
    let _ = &__done;
    if threads <= 1 {
        let mut local = seed;
        while let Some(addr) = local.pop() {
            scan(addr, &mut local);
        }
        return;
    }

    let shared = Mutex::new(DrainState {
        stack: seed,
        idle: 0,
        done: false,
    });
    let cv = Condvar::new();

    let worker = || {
        let mut local: Vec<usize> = Vec::new();
        loop {
            {
                let mut g = shared.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if g.done {
                        return;
                    }
                    let len = g.stack.len();
                    if len > 0 {
                        let take = len.min(ACQUIRE_CHUNK);
                        local.extend(g.stack.drain(len - take..));
                        break;
                    }
                    g.idle += 1;
                    if g.idle == threads {
                        // Every worker is out of work and the global stack is
                        // empty: the closure is complete.
                        g.done = true;
                        cv.notify_all();
                        return;
                    }
                    g = cv.wait(g).unwrap_or_else(|e| e.into_inner());
                    g.idle -= 1;
                }
            }
            while let Some(addr) = local.pop() {
                scan(addr, &mut local);
                if local.len() >= SPILL_HIGH {
                    let surplus = local.len() - SPILL_KEEP;
                    let mut g = shared.lock().unwrap_or_else(|e| e.into_inner());
                    g.stack.extend(local.drain(..surplus));
                    drop(g);
                    cv.notify_all();
                }
            }
        }
    };

    std::thread::scope(|s| {
        for _ in 1..threads {
            s.spawn(&worker);
        }
        worker();
    });
}

// ---------------------------------------------------------------------------
// Parallel span zeroing
// ---------------------------------------------------------------------------

/// Zero `spans` (`(offset, size)` relative to `base`) using `threads` workers.
///
/// The spans are the sweep's reclaimed regions: pairwise disjoint and sorted,
/// so the workers touch strictly separate memory and need no synchronisation.
///
/// # Safety
/// Every `(off, sz)` must lie inside the live from-space region at `base`, and
/// the spans must be pairwise disjoint.
pub(crate) unsafe fn zero_spans_parallel(base: usize, spans: &[(usize, usize)], threads: usize) {
    let zero_one = |&(off, sz): &(usize, usize)| {
        // SAFETY: caller guarantees `[base+off, base+off+sz)` lies inside the
        // live from-space region and that the spans are pairwise disjoint.
        unsafe { std::ptr::write_bytes((base + off) as *mut u8, 0, sz) };
    };
    if threads <= 1 || spans.len() < threads {
        spans.iter().for_each(zero_one);
        return;
    }
    // Split by BYTES, not by span count: one coalesced span can be orders of
    // magnitude larger than its neighbours.
    let total: usize = spans.iter().map(|&(_, sz)| sz).sum();
    let per = total / threads + 1;
    let mut slices: Vec<&[(usize, usize)]> = Vec::with_capacity(threads);
    let mut start = 0usize;
    let mut acc = 0usize;
    for i in 0..spans.len() {
        acc += spans[i].1;
        if acc >= per && slices.len() + 1 < threads {
            slices.push(&spans[start..=i]);
            start = i + 1;
            acc = 0;
        }
    }
    if start < spans.len() {
        slices.push(&spans[start..]);
    }
    std::thread::scope(|s| {
        for chunk in slices.iter().skip(1) {
            s.spawn(move || chunk.iter().for_each(zero_one));
        }
        if let Some(first) = slices.first() {
            first.iter().for_each(zero_one);
        }
    });
}

#[cfg(test)]
mod tests {

    #[test]
    fn object_start_bits_match_a_hash_set_exactly() {
        const BASE: usize = 0x1_0000;
        const SPAN: usize = 4096;
        let mut bits = ObjectStartBits::new(BASE, SPAN);
        let mut set = std::collections::HashSet::new();
        for k in [0usize, 1, 2, 7, 63, 64, 65, 511] {
            let addr = BASE + k * 8;
            assert!(bits.insert(addr), "{addr:#x} is in-span and aligned");
            set.insert(addr);
        }
        // Every 8-byte slot in the span must agree with the reference set.
        for k in 0..(SPAN / 8) {
            let addr = BASE + k * 8;
            assert_eq!(
                bits.contains(addr),
                set.contains(&addr),
                "membership disagrees at {addr:#x}"
            );
        }
        assert_eq!(bits.len(), set.len());
    }

    #[test]
    fn object_start_bits_reject_out_of_span_and_unaligned() {
        const BASE: usize = 0x1_0000;
        let mut bits = ObjectStartBits::new(BASE, 4096);
        // Below the base, at the end of the span, and past it.
        assert!(!bits.insert(BASE - 8));
        assert!(!bits.insert(BASE + 4096));
        assert!(!bits.contains(BASE - 8));
        assert!(!bits.contains(BASE + 4096));
        // An 8-byte-misaligned start would alias its neighbour's bit, so it is
        // refused rather than recorded: the caller diverts to the non-moving
        // sweep. Recording it would make `contains(BASE + 8)` true.
        assert!(!bits.insert(BASE + 12));
        assert!(!bits.contains(BASE + 12));
        assert!(!bits.contains(BASE + 8));
        assert_eq!(bits.len(), 0);
    }

    /// gc-genpause F1: the word-batching cursor must record exactly the bits
    /// per-address `insert` would, and must have published them by the time it
    /// is dropped.
    ///
    /// The batching is the whole point of the cursor and also its only risk:
    /// bits live in a register-held mask until the walk moves past the word,
    /// so a missed flush loses up to 63 object starts silently -- and a start
    /// this bitmap does not have is one `forward_object` refuses to relocate.
    #[test]
    fn the_batching_cursor_records_exactly_what_insert_would() {
        const BASE: usize = 0x40_0000;
        const SPAN: usize = 4096;
        // Addresses crossing several word boundaries (one word = 512 bytes),
        // several within one word, and revisits in DESCENDING order to prove
        // the cursor does not assume ascending input.
        let addrs: Vec<usize> = vec![
            BASE, BASE + 8, BASE + 16, BASE + 504, BASE + 512, BASE + 520,
            BASE + 1024, BASE + 1032, BASE + 2040, BASE + 2048, BASE + 4088,
            BASE + 512, BASE + 8, BASE + 3000,
        ];

        let direct = ObjectStartBits::new(BASE, SPAN);
        for &a in &addrs {
            assert!(direct.insert(a), "{a:#x} is in-span and aligned");
        }

        let batched = ObjectStartBits::new(BASE, SPAN);
        {
            let mut run = batched.run();
            for &a in &addrs {
                assert!(run.insert(a), "{a:#x} is in-span and aligned");
            }
            // Deliberately NOT flushed by hand: the drop below must do it.
        }

        assert_eq!(batched.len(), direct.len(), "start counts must agree");
        for off in (0..SPAN).step_by(8) {
            assert_eq!(
                batched.contains(BASE + off),
                direct.contains(BASE + off),
                "membership disagrees at +{off}"
            );
        }
    }

    /// The cursor must reject the same addresses `insert` rejects -- an
    /// unaligned or out-of-span start cannot be represented, and the caller
    /// treats that as a walk that did not complete.
    #[test]
    fn the_batching_cursor_rejects_what_insert_rejects() {
        const BASE: usize = 0x40_0000;
        let bits = ObjectStartBits::new(BASE, 4096);
        let mut run = bits.run();
        assert!(!run.insert(BASE + 4), "unaligned");
        assert!(!run.insert(BASE + 4096), "past the span");
        assert!(!run.insert(BASE - 8), "before the span");
        assert!(run.insert(BASE + 4088), "the last aligned slot is in-span");
    }

    #[test]
    fn object_start_bits_handle_an_empty_span() {
        let mut bits = ObjectStartBits::new(0x1_0000, 0);
        assert!(!bits.insert(0x1_0000));
        assert!(!bits.contains(0x1_0000));
        assert_eq!(bits.len(), 0);
    }

    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn bitmap_claims_each_address_once() {
        let bits = YoungMarkBits::new(0x1000, 4096);
        assert!(bits.try_mark(0x1000));
        assert!(!bits.try_mark(0x1000));
        assert!(bits.contains(0x1000));
        assert!(!bits.contains(0x1008));
        assert!(bits.try_mark(0x1008));
        assert!(bits.contains(0x1008));
    }

    #[test]
    fn bitmap_ignores_out_of_range() {
        let bits = YoungMarkBits::new(0x1000, 64);
        assert!(!bits.try_mark(0x0fff));
        assert!(!bits.try_mark(0x1040));
        assert!(!bits.contains(0x1040));
        assert!(bits.try_mark(0x1038));
    }

    #[test]
    fn bitmap_zero_span_is_inert() {
        let bits = YoungMarkBits::new(0x1000, 0);
        assert!(!bits.try_mark(0x1000));
        assert!(bits.collect_marked().is_empty());
    }

    #[test]
    fn collect_marked_is_ascending_and_complete() {
        let bits = YoungMarkBits::new(0, 8192);
        let want: Vec<usize> = vec![0, 8, 512, 4088, 4096, 8184];
        for &a in &want {
            assert!(bits.try_mark(a));
        }
        assert_eq!(bits.collect_marked(), want);
    }

    #[test]
    fn parallel_drain_visits_every_node_exactly_once() {
        // Synthetic graph: node i (0..N) points to 2i+1 and 2i+2.
        const N: usize = 50_000;
        let visits: Vec<AtomicUsize> = (0..N).map(|_| AtomicUsize::new(0)).collect();
        for threads in [1usize, 2, 4, 8] {
            for v in &visits {
                v.store(0, Ordering::Relaxed);
            }
            let bits = YoungMarkBits::new(0, N * 8);
            assert!(bits.try_mark(0));
            drain_parallel(vec![0], threads, |addr, work| {
                let i = addr / 8;
                visits[i].fetch_add(1, Ordering::Relaxed);
                for child in [2 * i + 1, 2 * i + 2] {
                    if child < N && bits.try_mark(child * 8) {
                        work.push(child * 8);
                    }
                }
            });
            assert_eq!(bits.collect_marked().len(), N, "threads={threads}");
            for (i, v) in visits.iter().enumerate() {
                assert_eq!(v.load(Ordering::Relaxed), 1, "node {i}, threads={threads}");
            }
        }
    }

    #[test]
    fn zero_spans_parallel_clears_every_span() {
        let mut buf = vec![0xAAu8; 64 * 1024];
        let base = buf.as_mut_ptr() as usize;
        let spans: Vec<(usize, usize)> = (0..32).map(|i| (i * 2048, 1024)).collect();
        for threads in [1usize, 2, 4, 8] {
            for b in buf.iter_mut() {
                *b = 0xAA;
            }
            // SAFETY: spans are disjoint sub-ranges of `buf`.
            unsafe { zero_spans_parallel(base, &spans, threads) };
            for (off, sz) in &spans {
                assert!(buf[*off..*off + *sz].iter().all(|&b| b == 0));
                assert!(buf[*off + *sz..*off + 2048].iter().all(|&b| b == 0xAA));
            }
        }
    }
}
