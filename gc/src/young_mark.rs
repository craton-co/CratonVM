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
    ///
    /// # gengc-mark 2026-09-20: refuses a MISALIGNED address
    ///
    /// [`Self::locate`] deliberately does NOT require 8-byte alignment: it
    /// floors the address into the 8-byte slot containing it, so a marginal
    /// conservative candidate that is not 8-aligned still RETAINS the slot it
    /// lands in. That rounding is the safe direction for `try_mark` (mark one
    /// extra base) and the UNSAFE direction here: `unmark(base + 3)` clears the
    /// bit belonging to `base`, and if `base` is a genuinely marked live object
    /// start it vanishes from `collect_marked` -- i.e. from `side_sorted`, which
    /// IS the sweep's live set. The sweep would then reclaim a live object.
    ///
    /// Every address the caller can prove object-INTERIOR came out of the
    /// arena's object grid and is therefore 8-aligned by construction
    /// (`gen_object_total_size` rounds every footprint up to 8), so refusing a
    /// misaligned one costs nothing real: it only declines to retire a phantom
    /// interior mark, which is a classification nicety, never retention.
    #[inline]
    pub(crate) fn unmark(&self, addr: usize) -> bool {
        if addr & 7 != 0 {
            // Would alias the containing slot's bit -- see the note above.
            return false;
        }
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

    /// The highest recorded object start at or below `addr`, if any.
    ///
    /// For the evacuator's refusal census: an address the walk did not record
    /// is either an INTERIOR word of an object it did record -- in which case
    /// the reference itself is wrong and the refusal is correct -- or a base
    /// the walk never reached. The distance to the nearest start below is what
    /// separates them.
    pub(crate) fn nearest_start_at_or_below(&self, addr: usize) -> Option<usize> {
        let off = addr.checked_sub(self.base)?;
        if off >= self.span {
            return None;
        }
        let mut bit = off >> 3;
        loop {
            let w = bit >> 6;
            let b = bit & 63;
            let word = self.words[w].load(Ordering::Relaxed);
            let masked = word & (u64::MAX >> (63 - b));
            if masked != 0 {
                let hi = 63 - masked.leading_zeros() as usize;
                return Some(self.base + (((w << 6) | hi) << 3));
            }
            if w == 0 {
                return None;
            }
            bit = (w << 6) - 1;
        }
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

    /// True when no start is recorded. O(words), same cost class as
    /// [`Self::len`] but short-circuiting on the first non-zero word.
    ///
    /// Added 2026-09-20 for `concurrent_mark`'s old-gen object-start bitmap,
    /// which replaced a `HashSet` whose `is_empty` was the sweep's
    /// "nothing to reclaim" gate.
    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.words.iter().all(|w| w.load(Ordering::Relaxed) == 0)
    }

    /// Intersect in place: keep only the starts set in BOTH bitmaps.
    ///
    /// The word-wise substitute for `HashSet::retain(|a| other.contains(a))`,
    /// added 2026-09-20 for `concurrent_mark::remark`, which narrows the
    /// cycle's sweep-eligibility snapshot to the objects still allocated at
    /// the remark pause.
    ///
    /// The two bitmaps must share a BASE, so that a word index means the same
    /// address in both; `other` may cover a LONGER span than `self`, which is
    /// the normal case for a snapshot taken earlier against a region that has
    /// since grown. The AND then runs over `self`'s words, which are the only
    /// ones that can hold a bit.
    ///
    /// Returns `false` and changes NOTHING when the bases differ or when
    /// `self` is the longer of the two — either means the region moved or
    /// shrank under the snapshot, and the word indices no longer agree. A
    /// caller that gets `false` must fail closed (the concurrent sweep empties
    /// the snapshot and reclaims nothing), never proceed on the un-narrowed
    /// set.
    pub(crate) fn retain_intersection(&self, other: &ObjectStartBits) -> bool {
        if self.base != other.base || self.span > other.span {
            return false;
        }
        debug_assert!(self.words.len() <= other.words.len());
        // `zip` stops at the shorter iterator, which is `self`'s by the guard
        // above, so every word of `self` is visited exactly once.
        for (mine, theirs) in self.words.iter().zip(other.words.iter()) {
            mine.fetch_and(theirs.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        true
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
    /// Workers still able to produce work.
    ///
    /// gengc-mark 2026-09-20: termination used to be `idle == threads`, which
    /// silently assumed every spawned worker reaches the idle check. A `scan`
    /// that PANICS -- and `scan` is the conservative young-object walk, which
    /// reads mutator-written headers and can trip a bounds assert on a
    /// corrupted one -- unwinds straight out of the worker closure without ever
    /// incrementing `idle`. The remaining workers then wait on `cv` for an
    /// `idle` count that can never be reached, `std::thread::scope` waits for
    /// them, and the VM hangs inside a GC pause instead of dying with the
    /// panic. Counting LIVE workers instead, and decrementing on every exit
    /// path via [`WorkerExit`], turns that hang back into the panic it is.
    live: usize,
    done: bool,
}

/// Decrements [`DrainState::live`] on every worker exit -- normal return,
/// early return, or unwind -- and completes the drain when the workers that
/// remain are all idle. See the `live` field for why this must be a guard and
/// not a statement at the end of the loop.
struct WorkerExit<'a> {
    shared: &'a Mutex<DrainState>,
    cv: &'a Condvar,
}

impl Drop for WorkerExit<'_> {
    fn drop(&mut self) {
        {
            let mut g = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            g.live -= 1;
            if g.live == 0 {
                g.done = true;
            }
        }
        // Unconditional: a parked peer is waiting for an `idle == live` that
        // this exit has just made reachable (or, on the panic path, for work
        // that is never coming). Waking every one of them lets each re-run the
        // termination test against the NEW `live` count and leave.
        self.cv.notify_all();
    }
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
/// Local stack depth at which a worker publishes surplus *when a peer is
/// already parked for want of work*.
///
/// gengc-mark 2026-09-20 (this is the shape of the "+153 % at four workers"
/// reading, not a proof of it -- nothing here was measured). Work only ever
/// became visible to a peer at [`SPILL_HIGH`] = 2048 local entries. A young
/// mark closure is usually shallow and narrow: a worker that holds, say, 300
/// gray objects never reaches 2048, so it drains them alone while the other
/// `threads - 1` workers sit in `cv.wait`. The drain then costs a thread
/// spawn, a bitmap allocation and a condvar round trip per worker and runs
/// single-threaded anyway -- strictly worse than `threads == 1`.
///
/// Publishing much earlier, but ONLY when `idle_hint` says someone is actually
/// waiting, keeps the uncontended case identical (no extra lock acquisitions
/// when every worker is busy) and turns the contended case into real
/// parallelism.
const SPILL_WHEN_PEER_IDLE: usize = 64;

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
        live: threads,
        done: false,
    });
    let cv = Condvar::new();
    // Lock-free mirror of `DrainState::idle`, so a busy worker can ask "is
    // anyone waiting on me?" on the scan path without taking the drain lock.
    // Maintained under that lock, so it never disagrees with `idle` except for
    // the instant between the two updates -- and a stale read only costs (or
    // saves) one early spill, never correctness.
    let idle_hint = AtomicUsize::new(0);

    let worker = || {
        // Registered FIRST: from here on, every exit path (including an
        // unwind out of `scan`) decrements `live` and wakes the peers.
        let _exit = WorkerExit {
            shared: &shared,
            cv: &cv,
        };
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
                    idle_hint.store(g.idle, Ordering::Relaxed);
                    // `>=`, and against `live` rather than the original worker
                    // count: a worker that has already left (normally or by
                    // unwinding) can never arrive at this check, so waiting for
                    // the full `threads` would park the survivors forever.
                    if g.idle >= g.live {
                        // Every LIVE worker is out of work and the global stack
                        // is empty: the closure is complete.
                        g.done = true;
                        cv.notify_all();
                        return;
                    }
                    g = cv.wait(g).unwrap_or_else(|e| e.into_inner());
                    g.idle -= 1;
                    idle_hint.store(g.idle, Ordering::Relaxed);
                }
            }
            while let Some(addr) = local.pop() {
                scan(addr, &mut local);
                let n = local.len();
                // Publish surplus at the high-water mark as before, and also as
                // soon as a peer is parked -- see `SPILL_WHEN_PEER_IDLE`.
                if n >= SPILL_HIGH
                    || (n >= SPILL_WHEN_PEER_IDLE && idle_hint.load(Ordering::Relaxed) > 0)
                {
                    // Never publish everything: a worker that hands away its
                    // whole stack immediately re-enters the acquisition path
                    // and the batch ping-pongs across the lock.
                    let keep = SPILL_KEEP.min(n / 2);
                    let surplus = n - keep;
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
        let bits = ObjectStartBits::new(BASE, SPAN);
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

    /// gengc-mark2 2026-09-20 — `retain_intersection` is the word-wise
    /// substitute for `HashSet::retain(|a| other.contains(a))` that
    /// `concurrent_mark::remark` uses to narrow its sweep-eligibility
    /// snapshot. It must agree with that `retain` exactly.
    #[test]
    fn retain_intersection_matches_hash_set_retain() {
        const BASE: usize = 0x1_0000;
        const SPAN: usize = 4096;
        let mine = ObjectStartBits::new(BASE, SPAN);
        let theirs = ObjectStartBits::new(BASE, SPAN);
        let mut mine_set = std::collections::HashSet::new();
        let theirs_set: std::collections::HashSet<usize> = [0usize, 2, 63, 64, 200]
            .iter()
            .map(|k| BASE + k * 8)
            .collect();
        for k in [0usize, 1, 2, 64, 65, 200, 300] {
            let addr = BASE + k * 8;
            assert!(mine.insert(addr));
            mine_set.insert(addr);
        }
        for &addr in &theirs_set {
            assert!(theirs.insert(addr));
        }

        assert!(mine.retain_intersection(&theirs));
        mine_set.retain(|a| theirs_set.contains(a));

        for k in 0..(SPAN / 8) {
            let addr = BASE + k * 8;
            assert_eq!(
                mine.contains(addr),
                mine_set.contains(&addr),
                "intersection disagrees with HashSet::retain at {addr:#x}"
            );
        }
        assert_eq!(mine.len(), mine_set.len());
        assert!(!mine.is_empty());
    }

    /// The snapshot may be SHORTER than the later walk — the old generation
    /// grows while a cycle is open — but never longer, and never at a
    /// different base. Both refusals must change nothing, because the caller
    /// fails closed on them.
    #[test]
    fn retain_intersection_refuses_mismatched_geometry() {
        const BASE: usize = 0x1_0000;
        let short = ObjectStartBits::new(BASE, 512);
        let long = ObjectStartBits::new(BASE, 4096);
        let elsewhere = ObjectStartBits::new(BASE + 4096, 512);
        assert!(short.insert(BASE));
        assert!(long.insert(BASE));

        // Shorter snapshot against a longer walk: allowed.
        assert!(short.retain_intersection(&long));
        assert!(short.contains(BASE));

        // Longer against shorter, and a different base: refused, unchanged.
        assert!(!long.retain_intersection(&short));
        assert!(long.contains(BASE), "a refusal must not modify the bitmap");
        assert!(!long.retain_intersection(&elsewhere));
        assert!(long.contains(BASE));
    }

    /// `is_empty` must agree with `len() == 0` on both sides.
    #[test]
    fn object_start_bits_is_empty_agrees_with_len() {
        const BASE: usize = 0x1_0000;
        let bits = ObjectStartBits::new(BASE, 4096);
        assert!(bits.is_empty());
        assert_eq!(bits.len(), 0);
        assert!(bits.insert(BASE + 64 * 8));
        assert!(!bits.is_empty());
        assert_eq!(bits.len(), 1);
        // A zero-span bitmap has no words at all and must still be empty.
        let none = ObjectStartBits::new(BASE, 0);
        assert!(none.is_empty());
        assert_eq!(none.len(), 0);
    }

    #[test]
    fn object_start_bits_reject_out_of_span_and_unaligned() {
        const BASE: usize = 0x1_0000;
        let bits = ObjectStartBits::new(BASE, 4096);
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
            BASE,
            BASE + 8,
            BASE + 16,
            BASE + 504,
            BASE + 512,
            BASE + 520,
            BASE + 1024,
            BASE + 1032,
            BASE + 2040,
            BASE + 2048,
            BASE + 4088,
            BASE + 512,
            BASE + 8,
            BASE + 3000,
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
        let bits = ObjectStartBits::new(0x1_0000, 0);
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

    /// gengc-mark 2026-09-20 — `unmark` must refuse a MISALIGNED address.
    ///
    /// `locate` floors an address into its containing 8-byte slot, which is the
    /// safe direction for `try_mark` and the unsafe one for `unmark`: clearing
    /// through `base + 3` would retire `base`'s bit, and `base` may be a
    /// genuinely marked live object start. Losing it from `collect_marked`
    /// loses it from `side_sorted`, which is the sweep's live set.
    #[test]
    fn unmark_refuses_a_misaligned_address() {
        let bits = YoungMarkBits::new(0x1000, 4096);
        assert!(bits.try_mark(0x1000));
        // A misaligned clear must not touch the aligned neighbour's bit.
        assert!(!bits.unmark(0x1001));
        assert!(!bits.unmark(0x1007));
        assert!(
            bits.contains(0x1000),
            "a misaligned unmark retired a live object's mark"
        );
        // The aligned clear still works, which is the only case the caller
        // (the late base-resolution pass) can actually produce.
        assert!(bits.unmark(0x1000));
        assert!(!bits.contains(0x1000));
    }

    /// gengc-mark 2026-09-20 — a `scan` that panics must not hang the drain.
    ///
    /// Termination used to be `idle == threads`, which assumed every worker
    /// reaches the idle check. A worker that unwinds out of `scan` never does,
    /// so the survivors parked on the condvar forever and `thread::scope`
    /// waited on them: a hang inside a GC pause instead of a panic. The drain
    /// must now propagate the panic promptly.
    ///
    /// A regression here manifests as this test HANGING, not failing.
    #[test]
    fn a_panicking_scan_terminates_the_drain_instead_of_hanging() {
        const POISON: usize = 777 * 8;
        let seed: Vec<usize> = (0..1000usize).map(|i| i * 8).collect();
        let prev = std::panic::take_hook();
        // The worker's panic is expected; don't spam the test output with it.
        std::panic::set_hook(Box::new(|_| {}));
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drain_parallel(seed, 4, |addr, _work| {
                assert_ne!(addr, POISON, "poisoned object");
            });
        }));
        std::panic::set_hook(prev);
        assert!(
            caught.is_err(),
            "the drain must propagate the worker's panic, not swallow it"
        );
    }

    /// gengc-mark 2026-09-20 — the early-spill path must not lose or duplicate
    /// work.
    ///
    /// A worker now publishes surplus as soon as a peer is parked
    /// (`SPILL_WHEN_PEER_IDLE`), not only at `SPILL_HIGH`. The graph below is
    /// shaped to cross that threshold repeatedly -- a spine of hub nodes each
    /// fanning out to 200 leaves -- so the drain takes the early-spill branch
    /// many times per run. Every node must still be scanned exactly once.
    #[test]
    fn early_spill_visits_every_node_exactly_once() {
        const HUBS: usize = 40;
        const FANOUT: usize = 200;
        const N: usize = HUBS * (FANOUT + 1);
        for threads in [1usize, 2, 4, 8] {
            let visits: Vec<AtomicUsize> = (0..N).map(|_| AtomicUsize::new(0)).collect();
            let bits = YoungMarkBits::new(0, N * 8);
            assert!(bits.try_mark(0));
            drain_parallel(vec![0], threads, |addr, work| {
                let i = addr / 8;
                visits[i].fetch_add(1, Ordering::Relaxed);
                // Hub `h` lives at index `h * (FANOUT + 1)`; it points at its
                // FANOUT leaves and at the next hub.
                if i % (FANOUT + 1) != 0 {
                    return; // leaf
                }
                for k in 1..=FANOUT {
                    let child = i + k;
                    if child < N && bits.try_mark(child * 8) {
                        work.push(child * 8);
                    }
                }
                let next_hub = i + FANOUT + 1;
                if next_hub < N && bits.try_mark(next_hub * 8) {
                    work.push(next_hub * 8);
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
