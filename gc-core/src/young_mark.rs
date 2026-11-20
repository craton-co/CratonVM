// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
//! * [`drain_parallel`] — a work-sharing drain of the mark worklist, with a
//!   global batch stack and per-worker local stacks. The collector runs it as
//!   [`drain_parallel_on`] on the heap's persistent `EvacPool`; scoped
//!   threads remain only for a caller without a pool (the tests).
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
//! * Every worker has finished before the caller reads the bitmap, giving the
//!   happens-before edge for the final `collect_marked`: `EvacPool::scope`'s
//!   completion barrier on the heap's persistent pool (the collector's
//!   callers, gce e1/y), or `std::thread::scope`'s join without one.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

/// One bit per 8 bytes of a young from-space region.
///
/// Addresses outside `[base, base + span)` are silently ignored (callers
/// already screen with `in_young`; this is belt-and-braces).
pub struct YoungMarkBits {
    words: *mut AtomicU64,
    nwords: usize,
    base: usize,
    span: usize,
    /// JIT round 23 wave 9, lane L3 (`CRATONVM_GEN_YOUNG_MARK_SUMMARY`, default
    /// ON): one bit per [`MARK_SUMMARY_BLOCK_WORDS`]-word block (one 4 KiB page
    /// of `words`) that any `try_mark` claimed a bit in; empty when the switch
    /// is off. A superset of the blocks holding a set bit (`unmark` leaves its
    /// block bit), so [`Self::collect_marked`] reads only those pages instead
    /// of every word of a bitmap sized by `used`.
    summary: Vec<AtomicU64>,
}

/// Wave 9: words per summary block -- 4 KiB of bitmap, 256 KiB of young.
const MARK_SUMMARY_BLOCK_WORDS: usize = 512;

/// JIT round 23 wave 9, lane L3: `CRATONVM_GEN_YOUNG_MARK_SUMMARY` (default
/// ON, `0` kills). The non-moving young sweep's side-mark bitmap keeps a
/// per-page summary, and `collect_marked` (the `side_sorted` build) visits only
/// the pages a mark touched. Without it, the build read every word of a bitmap
/// covering all of `used` -- 3.8 MB for a 245 MB eden, mostly untouched
/// zero-page mappings whose first read faults -- twice per cycle: `[gcphase]
/// mark: phase=2ms` of an 11 ms pause on `R21AllocLean lean-static-long16`
/// (Generational, n23-w8c) for 24.5 K marked objects. Read per bitmap.
fn gen_young_mark_summary_enabled() -> bool {
    true}

// SAFETY: every access to `words` goes through atomic operations on
// `AtomicU64`; the allocation is owned exclusively by this value and freed
// once in `Drop`.
unsafe impl Send for YoungMarkBits {}
// SAFETY: as above — all shared access is atomic.
unsafe impl Sync for YoungMarkBits {}

impl YoungMarkBits {
    /// Cover `[base, base + span)`. The backing store is `alloc_zeroed`, so a
    /// multi-megabyte bitmap costs a zero-page mapping rather than a memset.
    pub fn new(base: usize, span: usize) -> Self {
        let nbits = span.div_ceil(8);
        let nwords = nbits.div_ceil(64);
        if nwords == 0 {
            return Self {
                words: std::ptr::NonNull::<AtomicU64>::dangling().as_ptr(),
                nwords: 0,
                base,
                span: 0,
                summary: Vec::new(),
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
        let summary = if gen_young_mark_summary_enabled() {
            let blocks = nwords.div_ceil(MARK_SUMMARY_BLOCK_WORDS);
            (0..blocks.div_ceil(64)).map(|_| AtomicU64::new(0)).collect()
        } else {
            Vec::new()
        };
        Self {
            words,
            nwords,
            base,
            span,
            summary,
        }
    }

    /// Wave 9: note that word `w` may hold a bit (`CRATONVM_GEN_YOUNG_MARK_SUMMARY`).
    /// A load first, so the shared summary line is written once per block.
    #[inline]
    fn note_summary(&self, w: usize) {
        let blk = w / MARK_SUMMARY_BLOCK_WORDS;
        if let Some(slot) = self.summary.get(blk >> 6) {
            let mask = 1u64 << (blk & 63);
            if slot.load(Ordering::Relaxed) & mask == 0 {
                slot.fetch_or(mask, Ordering::Relaxed);
            }
        }
    }

    /// Wave 9: the word ranges `collect_marked` reads -- every summary block
    /// with its bit set, or the whole bitmap when there is no summary.
    fn marked_word_ranges(&self) -> Vec<(usize, usize)> {
        if self.summary.is_empty() {
            return if self.nwords == 0 {
                Vec::new()
            } else {
                vec![(0, self.nwords)]
            };
        }
        let mut out: Vec<(usize, usize)> = Vec::new();
        for (i, slot) in self.summary.iter().enumerate() {
            let mut bits = slot.load(Ordering::Relaxed);
            while bits != 0 {
                let blk = i * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let lo = blk * MARK_SUMMARY_BLOCK_WORDS;
                let hi = (lo + MARK_SUMMARY_BLOCK_WORDS).min(self.nwords);
                if lo >= hi {
                    continue;
                }
                match out.last_mut() {
                    Some(last) if last.1 == lo => last.1 = hi,
                    _ => out.push((lo, hi)),
                }
            }
        }
        out
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
    pub fn try_mark(&self, addr: usize) -> bool {
        match self.locate(addr) {
            None => false,
            Some((w, mask)) => {
                // SAFETY: `locate` bounds-checked `addr`, so `w < nwords`.
                let prev = unsafe { (*self.words.add(w)).fetch_or(mask, Ordering::Relaxed) };
                let claimed = prev & mask == 0;
                // Wave 9: before any reader runs (the drain's join is the
                // happens-before edge `collect_marked` already relies on).
                if claimed {
                    self.note_summary(w);
                }
                claimed
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
    pub fn unmark(&self, addr: usize) -> bool {
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
    pub fn contains(&self, addr: usize) -> bool {
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
    ///
    /// gci i2/st (2026-10-12): sized by a `count_ones` pass first. This runs
    /// on the collecting thread alone, after the parallel mark, and on a young
    /// generation that is mostly live it pushes millions of addresses: an
    /// unsized `Vec` then regrew about twenty times, copying up to its final
    /// size again and freeing (`VirtualFree`) each old buffer inside the pause.
    /// The driver of the stalled javac pauses was sampled in this loop in 6 of
    /// 16 watchdog dumps (`gci-i2st-...-progressing-pause-regime-...` page). The counting pass reads the
    /// same words the collecting pass reads, once more, and allocates nothing.
    ///
    /// JIT round 23 wave 9 (`CRATONVM_GEN_YOUNG_MARK_SUMMARY`): both passes
    /// read only the summary's blocks ([`Self::marked_word_ranges`]), in
    /// ascending order, so the output is the same vector.
    pub fn collect_marked(&self) -> Vec<usize> {
        let ranges = self.marked_word_ranges();
        let mut marked = 0usize;
        for &(lo, hi) in &ranges {
            for w in lo..hi {
                // SAFETY: `hi <= nwords`.
                let word = unsafe { (*self.words.add(w)).load(Ordering::Relaxed) };
                marked += word.count_ones() as usize;
            }
        }
        let mut out = Vec::with_capacity(marked);
        for &(lo, hi) in &ranges {
            for w in lo..hi {
                // SAFETY: `hi <= nwords`.
                let mut word = unsafe { (*self.words.add(w)).load(Ordering::Relaxed) };
                while word != 0 {
                    let b = word.trailing_zeros() as usize;
                    word &= word - 1;
                    out.push(self.base + ((w * 64 + b) << 3));
                }
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
pub struct StartRun<'a> {
    bits: &'a ObjectStartBits,
    word: usize,
    mask: u64,
}

impl StartRun<'_> {
    /// Record an object start. Same contract as [`ObjectStartBits::insert`]:
    /// `false` means the address is outside the span or not 8-byte aligned,
    /// and the caller must not run a moving cycle against this bitmap.
    #[inline]
    pub fn insert(&mut self, addr: usize) -> bool {
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
    pub fn flush(&mut self) {
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
pub struct ObjectStartBits {
    words: Vec<AtomicU64>,
    base: usize,
    span: usize,
    /// JIT round 23 wave 4, lane L4 (`CRATONVM_GEN_LAZY_YOUNG_OBJECT_STARTS`):
    /// `Some` when the bitmap is filled one chunk at a time on demand (see
    /// [`LazyStartGrid`]); `None` for every eagerly-filled bitmap, whose
    /// readers are unchanged.
    lazy: Option<Box<LazyStartGrid>>,
}

impl ObjectStartBits {
    /// Cover `[base, base + span)`. The words come from `alloc_zeroed`, so a
    /// multi-megabyte bitmap costs a zero-page mapping, not a memset.
    ///
    /// gen r4/mark (2026-09-23): the doc above used to say "`vec![0u64; n]`
    /// allocates zeroed" while the body was `(0..n).map(|_| AtomicU64::new(0))
    /// .collect()` — which is NOT the `vec!` zero specialisation (that needs
    /// `Clone` + `IsZero`, and `AtomicU64` is neither): it is a plain
    /// allocation followed by a store to every word, i.e. exactly the memset,
    /// touching every page of a bitmap that the moving young cycle and every
    /// old-gen object-start walk (`concurrent_mark`) allocate per collection,
    /// and of which most words stay zero. Now it does what it said.
    pub fn new(base: usize, span: usize) -> Self {
        let nwords = span.div_ceil(8).div_ceil(64);
        Self {
            words: zeroed_atomic_words(nwords),
            base,
            span,
            lazy: None,
        }
    }

    /// JIT round 23 wave 4, lane L4: a bitmap over `[base, base + span)` whose
    /// chunks `[bounds[i], bounds[i + 1])` (offsets) are walked by `walker`
    /// the first time anything asks about an address in them. `None` when the
    /// grid is unusable (fewer than one chunk, not starting at 0, not ending at
    /// `span`, not strictly increasing): the caller then walks eagerly.
    pub fn new_lazy(
        base: usize,
        span: usize,
        bounds: Vec<usize>,
        skips: Vec<(usize, usize)>,
        walker: StartChunkWalker,
    ) -> Option<Self> {
        if bounds.last() != Some(&span) {
            return None;
        }
        let grid = LazyStartGrid::new(bounds, skips, walker)?;
        let mut bits = Self::new(base, span);
        bits.lazy = Some(Box::new(grid));
        Some(bits)
    }

    /// Whether this bitmap is filled on demand ([`Self::new_lazy`]).
    #[inline]
    pub fn is_lazy(&self) -> bool {
        self.lazy.is_some()
    }

    /// Lazy grid only: the number of chunks. `0` for an eager bitmap.
    pub fn lazy_chunk_count(&self) -> usize {
        self.lazy.as_deref().map_or(0, |l| l.state.len())
    }

    /// Lazy grid only: PROVE chunk `i` -- walk it and every chunk below it
    /// not yet proven, in order (wave 4c's induction). `true` when its starts
    /// are all recorded and its anchor is proven; `false` when it or a chunk
    /// below cannot be (fatal once sealed) or `i` is out of range. An eager
    /// bitmap answers `true`.
    pub fn ensure_chunk_index(&self, i: usize) -> bool {
        match self.lazy.as_deref() {
            None => true,
            Some(l) => i < l.state.len() && l.ensure_chunk(self, i),
        }
    }

    /// Lazy grid only: make sure the chunk holding `addr` is walked. `true`
    /// for an eager bitmap, for an address outside the span (no chunk holds
    /// it, nothing to prove), and for a chunk whose walk succeeded; `false`
    /// only when the chunk's walk failed.
    pub fn ensure_addr(&self, addr: usize) -> bool {
        let Some(l) = self.lazy.as_deref() else {
            return true;
        };
        let Some(off) = addr.checked_sub(self.base) else {
            return true;
        };
        match l.chunk_of(off) {
            None => true,
            Some(c) => l.ensure_chunk(self, c),
        }
    }

    /// Lazy grid only: make sure every chunk overlapping `[lo, hi)`
    /// (addresses) is walked. `false` when any of them failed.
    pub fn ensure_span(&self, lo: usize, hi: usize) -> bool {
        let Some(l) = self.lazy.as_deref() else {
            return true;
        };
        let lo_off = lo.saturating_sub(self.base);
        let hi_off = hi.saturating_sub(self.base).min(self.span);
        if lo_off >= hi_off {
            return true;
        }
        let Some(mut c) = l.chunk_of(lo_off) else {
            return true;
        };
        let mut ok = true;
        while c < l.state.len() && l.bounds[c] < hi_off {
            ok &= l.ensure_chunk(self, c);
            c += 1;
        }
        ok
    }

    /// Lazy grid only: `(chunks, walked, failed, bytes walked)`, for the
    /// pause census. `None` for an eager bitmap.
    pub fn lazy_census(&self) -> Option<(usize, usize, usize, usize)> {
        self.lazy.as_deref().map(|l| {
            (
                l.state.len(),
                l.chunks_walked.load(Ordering::Relaxed),
                l.chunks_failed.load(Ordering::Relaxed),
                l.bytes_walked.load(Ordering::Relaxed),
            )
        })
    }

    /// Clear every bit for the offsets `[lo, hi)`. A failed lazy chunk's
    /// partial record is withdrawn this way before its FAILED state is
    /// published, so no reader ever sees a bit a refused chain set.
    fn clear_offsets(&self, lo: usize, hi: usize) {
        let hi = hi.min(self.span);
        if lo >= hi {
            return;
        }
        let first = lo.div_ceil(8);
        let end = hi.div_ceil(8);
        let mut bit = first;
        while bit < end {
            let w = bit >> 6;
            let b = bit & 63;
            let n = (64 - b).min(end - bit);
            let ones = if n == 64 {
                u64::MAX
            } else {
                ((1u64 << n) - 1) << b
            };
            self.words[w].fetch_and(!ones, Ordering::Relaxed);
            bit += n;
        }
    }

    /// Walk every chunk a lazy bitmap has not walked yet (sequentially).
    /// Readers that need the whole picture (`duplicate`, `truncate`,
    /// `retain_intersection`, `is_empty`) call it first; a failed chunk just
    /// contributes no bits.
    fn ensure_all(&self) {
        if let Some(l) = self.lazy.as_deref() {
            let n = l.state.len();
            if !l.induction {
                // Certified anchors: every chunk answers on its own.
                for c in 0..n {
                    l.ensure_chunk(self, c);
                }
                return;
            }
            if !l.ensure_chunk(self, n - 1) {
                // Wave 4c: speculative walks above the first unprovable chunk
                // may have left bits; a whole-bitmap reader must not see them.
                let broken = l.broken.load(Ordering::Acquire);
                if let Some(&lo) = l.bounds.get(broken) {
                    self.clear_offsets(lo, self.span);
                }
            }
        }
    }

    /// Wave 4c: walk chunk `i` SPECULATIVELY (no induction: its bits answer
    /// nothing until [`Self::ensure_chunk_index`] proves the prefix up to it).
    /// The parallel provers run this on many chunks at once, then prove the
    /// prefix on one thread. `true` for an eager bitmap.
    pub fn walk_chunk_index(&self, i: usize) -> bool {
        match self.lazy.as_deref() {
            None => true,
            Some(l) => i < l.state.len() && l.walk_chunk(self, i),
        }
    }

    /// Wave 4c: prove chunks by induction from offset 0 (`true`) instead of
    /// trusting the arena-certified anchors (`false`, the default). Before any
    /// question is asked. No-op on an eager bitmap.
    pub fn set_lazy_induction(&mut self, on: bool) {
        if let Some(l) = self.lazy.as_deref_mut() {
            l.induction = on;
        }
    }

    /// Wave 4c: whether this lazy grid proves by induction
    /// ([`Self::set_lazy_induction`]). `false` for an eager bitmap.
    pub fn lazy_induction(&self) -> bool {
        self.lazy.as_deref().is_some_and(|l| l.induction)
    }

    /// Wave 4c: the merged walks this grid ran (a chunk whose chain missed its
    /// end bound, walked on to the next DONE chunk or the span's end). `0` for
    /// an eager bitmap.
    pub fn lazy_merged_walks(&self) -> usize {
        self.lazy
            .as_deref()
            .map_or(0, |l| l.merged_walks.load(Ordering::Relaxed))
    }

    /// Wave 4c: the chunk index holding `addr`, `None` for an eager bitmap or
    /// an address outside the span.
    pub fn lazy_chunk_of_addr(&self, addr: usize) -> Option<usize> {
        self.lazy.as_deref()?.chunk_of(addr.checked_sub(self.base)?)
    }

    /// Wave 4c: `true` from the moment the cycle forwards anything until it
    /// stops (before the in-place finish's poisoning, which has its own
    /// memset fallback). While sealed, an unprovable chunk is fatal instead
    /// of "not an object start" -- see `LazyStartGrid::sealed`. No-op on an
    /// eager bitmap.
    pub fn seal_lazy(&self, sealed: bool) {
        if let Some(l) = self.lazy.as_deref() {
            l.sealed.store(sealed, Ordering::Release);
        }
    }

    /// Narrow a frozen set to its recorded prefix. The caller must not
    /// discard a recorded start; this changes only the representable extent.
    pub fn truncate(&mut self, span: usize) {
        self.ensure_all();
        self.lazy = None;
        assert!(span <= self.span);
        self.words.truncate(span.div_ceil(8).div_ceil(64));
        self.span = span;
    }

    /// Independent snapshot; callers freeze all writers while copying.
    pub fn duplicate(&self) -> Self {
        // A lazy bitmap is completed first; the copy is an eager one.
        self.ensure_all();
        Self {
            words: self
                .words
                .iter()
                .map(|w| AtomicU64::new(w.load(Ordering::Relaxed)))
                .collect(),
            base: self.base,
            span: self.span,
            lazy: None,
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
    pub fn insert(&self, addr: usize) -> bool {
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
    pub fn extent(&self) -> (usize, usize) {
        (self.base, self.span)
    }

    /// The highest recorded object start at or below `addr`, if any.
    ///
    /// For the evacuator's refusal census: an address the walk did not record
    /// is either an INTERIOR word of an object it did record -- in which case
    /// the reference itself is wrong and the refusal is correct -- or a base
    /// the walk never reached. The distance to the nearest start below is what
    /// separates them.
    pub fn nearest_start_at_or_below(&self, addr: usize) -> Option<usize> {
        let off = addr.checked_sub(self.base)?;
        if off >= self.span {
            return None;
        }
        if let Some(l) = self.lazy.as_deref() {
            // Chunk by chunk downward: walk the chunk, answer from it if it
            // holds a start at or below the query, else step to the one below.
            // A failed chunk holds no bits and is stepped over.
            let mut c = l.chunk_of(off)?;
            loop {
                if l.ensure_chunk(self, c) {
                    let q = off.min(l.bounds[c + 1] - 1);
                    if let Some(s) = self.raw_nearest_at_or_below(q) {
                        if s >= self.base + l.bounds[c] {
                            return Some(s);
                        }
                    }
                }
                if c == 0 {
                    return None;
                }
                c -= 1;
            }
        }
        self.raw_nearest_at_or_below(off)
    }

    /// [`Self::nearest_start_at_or_below`] over the recorded bits only, from
    /// the in-span OFFSET `off`.
    fn raw_nearest_at_or_below(&self, off: usize) -> Option<usize> {
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

    /// The lowest recorded object start at or above `addr`, if any.
    ///
    /// gen r4w5/pinned5 (2026-09-24): the forward twin of
    /// [`Self::nearest_start_at_or_below`], for the pinned young copy's page
    /// resolution — every object overlapping a pinned 4 KiB page is the one
    /// straddling into it from below plus every start inside it, and the
    /// starts inside it come from here. An `addr` off the 8-byte grid rounds
    /// UP to it (a start cannot lie between two grid points). `addr` below
    /// the covered span answers the first start in the span.
    pub fn next_start_at_or_above(&self, addr: usize) -> Option<usize> {
        let off = addr.saturating_sub(self.base);
        if off >= self.span {
            return None;
        }
        if let Some(l) = self.lazy.as_deref() {
            // Chunk by chunk upward, each scan bounded by its chunk's end so
            // a run of empty (or failed) chunks is not rescanned per step.
            let mut c = l.chunk_of(off)?;
            let mut from = off;
            while c < l.state.len() {
                let end = l.bounds[c + 1];
                if l.ensure_chunk(self, c) {
                    if let Some(s) = self.raw_next_start_in(from, end) {
                        return Some(s);
                    }
                }
                from = end;
                c += 1;
            }
            return None;
        }
        self.raw_next_start_in(off, self.span)
    }

    /// Wave 4b: `pred` over every RECORDED start at an in-span offset in
    /// `[lo, hi)`, ascending; `false` at the first start it rejects. Reads
    /// the bits as they are -- never walks a lazy chunk -- so a lazy chunk's
    /// own walker may call it on the chunk it is walking.
    pub fn all_recorded_starts_in(
        &self,
        lo: usize,
        hi: usize,
        mut pred: impl FnMut(usize) -> bool,
    ) -> bool {
        let mut from = lo;
        while let Some(s) = self.raw_next_start_in(from, hi) {
            if !pred(s) {
                return false;
            }
            from = s - self.base + 8;
        }
        true
    }

    /// The lowest recorded start at an in-span offset in `[off, end)`.
    fn raw_next_start_in(&self, off: usize, end: usize) -> Option<usize> {
        let nbits = end.min(self.span).div_ceil(8);
        let mut bit = off.div_ceil(8);
        while bit < nbits {
            let w = bit >> 6;
            let b = bit & 63;
            // `b` is in 0..=63, so the shift never overflows.
            let word = self.words[w].load(Ordering::Relaxed) & (u64::MAX << b);
            if word != 0 {
                let found = (w << 6) | word.trailing_zeros() as usize;
                if found >= nbits {
                    return None;
                }
                return Some(self.base + (found << 3));
            }
            bit = (w + 1) << 6;
        }
        None
    }

    /// Membership. An address outside the span, or not 8-byte aligned, is not
    /// an object start — the same answer the `FxHashSet` gave.
    #[inline]
    pub fn contains(&self, addr: usize) -> bool {
        match self.locate(addr) {
            None => false,
            Some((w, mask)) => {
                // Lazy grid: the answer is gated on the CHUNK STATE, never on
                // the bit alone -- a bit seen while another thread is still
                // walking the chunk would let a caller forward an object whose
                // neighbours that walk has yet to parse. `ensure_chunk`'s
                // Acquire on DONE also orders the walker's Relaxed bit sets
                // before the load below. A failed chunk answers false: the
                // bitmap's fail-closed direction ("not an object start").
                if let Some(l) = self.lazy.as_deref() {
                    match l.chunk_of(addr - self.base) {
                        Some(c) if l.ensure_chunk(self, c) => {}
                        _ => return false,
                    }
                }
                self.words[w].load(Ordering::Relaxed) & mask != 0
            }
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
    pub fn len(&self) -> usize {
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
    pub fn is_empty(&self) -> bool {
        self.ensure_all();
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
    pub fn retain_intersection(&self, other: &ObjectStartBits) -> bool {
        if self.base != other.base || self.span > other.span {
            return false;
        }
        self.ensure_all();
        other.ensure_all();
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
    pub fn run(&self) -> StartRun<'_> {
        StartRun {
            bits: self,
            word: usize::MAX,
            mask: 0,
        }
    }
}

/// JIT round 23 wave 4, lane L4: the per-chunk walker a lazy
/// [`ObjectStartBits`] calls -- `(base, lo, hi, skips, bits)`, offsets from
/// `base`, `true` when the chunk's chain was proven (landed exactly on `hi`)
/// and every start in it recorded. The moving collector passes
/// `gen_heap::objstart_chunk`'s wrapper; the tests pass synthetic walkers.
pub type StartChunkWalker =
    fn(usize, usize, usize, &[(usize, usize)], &ObjectStartBits) -> bool;

const LAZY_UNWALKED: u8 = 0;
const LAZY_WALKING: u8 = 1;
const LAZY_DONE: u8 = 2;
const LAZY_FAILED: u8 = 3;
/// Wave 4c: the chunk's own chain failed AND the merged walk from its anchor
/// (`LazyStartGrid::walk_merged_from`) failed too: unprovable, not retried.
const LAZY_DEAD: u8 = 4;
/// Granule of the chunk lookup table: `chunk_of` starts from the first chunk
/// overlapping the 64 KiB granule and steps forward (chunks are at least the
/// collector's lazy stride wide, so the step count is small).
const LAZY_GRANULE_SHIFT: u32 = 16;

/// JIT round 23 wave 4, lane L4 (`CRATONVM_GEN_LAZY_YOUNG_OBJECT_STARTS`,
/// proposal 9 item 1): the per-chunk state of an on-demand object-start
/// bitmap.
///
/// The eager walk chases every allocated header of from-space before a moving
/// young cycle forwards anything (8-10 ms of a ~26 ms pause on
/// `R21AllocLean`'s Generational rows, O(allocated) on a ~1 MB live set). The
/// chunk grid is the parallel walk's: allocator anchors plus the ends of the
/// free / TLAB skip blocks, each chunk proven by its chain landing exactly on
/// the next bound. Lazily, a chunk is walked the first time anything asks
/// about an address in it, and every answer about that chunk waits for its
/// walk:
///
/// * `UNWALKED -> WALKING` is claimed by one CAS; the claimant walks, and on
///   failure clears the chunk's bits BEFORE publishing `FAILED`; the state
///   store is `Release`, every reader's load `Acquire`, so the Relaxed bit
///   sets are visible to whoever sees `DONE`. Others spin (then yield) while
///   it is `WALKING`.
/// * An object in a chunk is forwarded only after `contains` said yes, i.e.
///   after the chunk's walk finished, so no walk ever parses a header a
///   forward already rewrote.
/// * Wave 4c (review `r23-w4b-review-lazy-young-starts-lose-the-anchor-induction`):
///   a chunk ANSWERS only from a boundary that is known to be one. By default
///   the arena CERTIFIES every anchor (`Arena::hand_out` forgets each anchor
///   strictly inside a hand-out, so an anchor is a live hand-out base, a free
///   block's start or outside `used`, and the collector drops anchors strictly
///   inside skip blocks), so a chunk answers once its own chain lands. With
///   `induction` (`CRATONVM_GEN_LAZY_YOUNG_STARTS_INDUCTION`), it answers only
///   once it and every chunk below it walked and landed -- the eager walk's
///   own proof from offset 0 (`prefix`), anchors trusted for nothing.
/// * A chain that misses its END bound (a bound that is not a boundary) is
///   walked on as one merged chain to the next DONE chunk or the span's end
///   (`walk_merged_from`) -- the eager walk's sequential fallback, over the
///   smallest suffix it can be.
/// * Before the cycle forwards anything, an unprovable chunk answers "not an
///   object start" and the collector skips the cycle (the eager divert). The
///   collector proves, before forwarding, the prefix up to every chunk a
///   root / pin / conservative / dirty-card word names, then SEALS the grid:
///   from then on an unprovable chunk (one first reached through an object
///   field) is a fatal stop, never a refusal that would leave a reference
///   into from-space across its reset.
pub struct LazyStartGrid {
    /// Chunk bounds as offsets: `bounds[0] == 0`, strictly increasing, the
    /// last one the bitmap's span.
    bounds: Vec<usize>,
    /// The free / un-retired-TLAB skip blocks, `(offset, len)`, ascending.
    skips: Vec<(usize, usize)>,
    state: Vec<std::sync::atomic::AtomicU8>,
    /// For granule `g`, the first chunk whose range holds offset `g << 16`.
    granule_first: Vec<u32>,
    walker: StartChunkWalker,
    chunks_walked: AtomicUsize,
    chunks_failed: AtomicUsize,
    bytes_walked: AtomicUsize,
    /// Wave 4c (review `r23-w4b-...-lose-the-anchor-induction`): the number of
    /// LEADING chunks proven -- every chunk below it walked and landed, so
    /// each of their bounds is a genuine start by induction from offset 0,
    /// exactly as the eager walk proves them. A chunk answers only once it is
    /// below this prefix; a walk of a chunk above it is speculative.
    prefix: AtomicUsize,
    /// The lowest chunk whose walk failed (`usize::MAX`: none). Every chunk
    /// at or above it is unprovable.
    broken: AtomicUsize,
    /// Set once the cycle starts forwarding (`ObjectStartBits::seal_lazy`):
    /// from then on an unprovable chunk can no longer divert the cycle, and
    /// answering "not an object start" would leave a precise reference in
    /// from-space across its reset -- so it is a fatal heap-layout error.
    sealed: std::sync::atomic::AtomicBool,
    /// Wave 4c: `true` = prove every chunk by induction from offset 0
    /// (`ensure_prefix`); `false` (the collector's default) = trust each
    /// chunk's anchor as an object boundary, which the arena CERTIFIES
    /// (`Arena::hand_out` forgets every anchor strictly inside each hand-out,
    /// so an anchor is always a hand-out base, a free-block start or outside
    /// `used`; see `gen_heap::lazy_young_start_bounds`). Set by
    /// [`ObjectStartBits::set_lazy_induction`].
    induction: bool,
    /// Serialises the merged walks ([`Self::walk_merged_from`]).
    merge: std::sync::Mutex<()>,
    /// Merged walks run (a chunk whose chain missed its END bound).
    merged_walks: AtomicUsize,
}

impl LazyStartGrid {
    pub fn new(
        bounds: Vec<usize>,
        skips: Vec<(usize, usize)>,
        walker: StartChunkWalker,
    ) -> Option<Self> {
        if bounds.len() < 2 || bounds[0] != 0 || !bounds.windows(2).all(|w| w[0] < w[1]) {
            return None;
        }
        let nchunks = bounds.len() - 1;
        if nchunks > u32::MAX as usize {
            return None;
        }
        let span = bounds[nchunks];
        let ngran = (span >> LAZY_GRANULE_SHIFT) + 1;
        let mut granule_first = Vec::with_capacity(ngran);
        let mut c = 0usize;
        for g in 0..ngran {
            let off = g << LAZY_GRANULE_SHIFT;
            while c + 1 < nchunks && bounds[c + 1] <= off {
                c += 1;
            }
            granule_first.push(c as u32);        }
        Some(Self {
            bounds,
            skips,
            state: (0..nchunks)
                .map(|_| std::sync::atomic::AtomicU8::new(LAZY_UNWALKED))
                .collect(),
            granule_first,
            walker,
            chunks_walked: AtomicUsize::new(0),
            chunks_failed: AtomicUsize::new(0),
            bytes_walked: AtomicUsize::new(0),
            prefix: AtomicUsize::new(0),
            broken: AtomicUsize::new(usize::MAX),
            sealed: std::sync::atomic::AtomicBool::new(false),
            induction: false,
            merge: std::sync::Mutex::new(()),
            merged_walks: AtomicUsize::new(0),
        })
    }

    /// Is chunk `c` answerable? See [`Self::induction`]: by induction from
    /// offset 0 ([`Self::ensure_prefix`]), or on its certified anchor
    /// ([`Self::ensure_single`]).
    fn ensure_chunk(&self, bits: &ObjectStartBits, c: usize) -> bool {
        if self.induction {
            self.ensure_prefix(bits, c)
        } else {
            self.ensure_single(bits, c)
        }
    }

    /// Wave 4c, certified anchors: chunk `c` answers once its own chain from
    /// `bounds[c]` -- a certified boundary -- landed, directly or through a
    /// merged walk past a bound the chain missed.
    fn ensure_single(&self, bits: &ObjectStartBits, c: usize) -> bool {
        if self.walk_chunk(bits, c) || self.walk_merged_from(bits, c) {
            return true;
        }
        self.broken.fetch_min(c, Ordering::AcqRel);
        self.unprovable(c)
    }

    /// Wave 4c: is chunk `c` PROVEN (walked, and every chunk below it walked
    /// and landed)? Proves it if not, extending the proven prefix in order --
    /// the eager walk's induction from offset 0, paid lazily and at most once
    /// per chunk. `false` when `c` or a chunk below it cannot be proven; once
    /// [`Self::sealed`] that is fatal (see the field).
    fn ensure_prefix(&self, bits: &ObjectStartBits, c: usize) -> bool {
        let mut i = self.prefix.load(Ordering::Acquire);
        if c < i {
            return true;
        }
        if c >= self.broken.load(Ordering::Acquire) {
            return self.unprovable(c);
        }
        while i <= c {
            if !self.walk_chunk(bits, i) && !self.walk_merged_from(bits, i) {
                self.broken.fetch_min(i, Ordering::AcqRel);
                return self.unprovable(c);
            }
            // AcqRel: a reader that sees the prefix past `i` also sees the
            // bits `walk_chunk`'s DONE published.
            self.prefix.fetch_max(i + 1, Ordering::AcqRel);
            i += 1;
        }
        true
    }

    /// Wave 4c: the answer for an unprovable chunk -- `false` before the
    /// cycle forwards anything (the caller diverts, as for a failed eager
    /// walk); a fatal stop after (`sealed`).
    fn unprovable(&self, c: usize) -> bool {
        if self.sealed.load(Ordering::Acquire) {
            let broken = self.broken.load(Ordering::Acquire);
            eprintln!(
                "[gc] FATAL (CRATONVM_GEN_LAZY_YOUNG_OBJECT_STARTS): young from-space chunk {c} \
                 cannot be proven during the evacuation (first unprovable chunk {broken}, \
                 offset 0x{:x}); its layout is not what the allocator wrote. Answering \
                 'not an object start' here would leave a reference into from-space across \
                 its reset, so the VM stops. Run without the switch to get the eager walk's \
                 skip-the-cycle answer.",
                self.bounds.get(broken).copied().unwrap_or(0),
            );
            std::process::abort();
        }
        false
    }

    /// The chunk holding offset `off`, `None` at or past the span.
    #[inline]
    fn chunk_of(&self, off: usize) -> Option<usize> {
        let span = *self.bounds.last()?;
        if off >= span {
            return None;
        }
        let mut c = *self.granule_first.get(off >> LAZY_GRANULE_SHIFT)? as usize;
        while self.bounds[c + 1] <= off {
            c += 1;
        }
        Some(c)
    }

    /// Walk chunk `c` if nobody has; wait while somebody is. `true` = DONE:
    /// the chain from `bounds[c]` landed on `bounds[c + 1]`. That proves
    /// nothing about `bounds[c]` itself -- [`Self::ensure_chunk`] adds the
    /// induction; this is the speculative half the parallel provers run.
    fn walk_chunk(&self, bits: &ObjectStartBits, c: usize) -> bool {
        /// Publishes FAILED if the walker unwinds, so no waiter spins forever.
        struct FailOnUnwind<'a> {
            state: &'a std::sync::atomic::AtomicU8,
            armed: bool,
        }
        impl Drop for FailOnUnwind<'_> {
            fn drop(&mut self) {
                if self.armed {
                    self.state.store(LAZY_FAILED, Ordering::Release);
                }
            }
        }
        let st = &self.state[c];
        let mut spins = 0u32;
        loop {
            match st.load(Ordering::Acquire) {
                LAZY_DONE => return true,
                LAZY_FAILED | LAZY_DEAD => return false,
                LAZY_UNWALKED => {
                    if st
                        .compare_exchange(
                            LAZY_UNWALKED,
                            LAZY_WALKING,
                            Ordering::Acquire,
                            Ordering::Relaxed,
                        )
                        .is_err()
                    {
                        continue;
                    }
                    let mut guard = FailOnUnwind {
                        state: st,
                        armed: true,
                    };
                    let (lo, hi) = (self.bounds[c], self.bounds[c + 1]);
                    let ok = (self.walker)(bits.base, lo, hi, &self.skips, bits);
                    if ok {
                        self.chunks_walked.fetch_add(1, Ordering::Relaxed);
                        self.bytes_walked.fetch_add(hi - lo, Ordering::Relaxed);
                    } else {
                        // Withdraw whatever the refused chain recorded before
                        // anyone can read it (see the type doc).
                        bits.clear_offsets(lo, hi);
                        self.chunks_failed.fetch_add(1, Ordering::Relaxed);
                    }
                    guard.armed = false;
                    st.store(if ok { LAZY_DONE } else { LAZY_FAILED }, Ordering::Release);
                    return ok;
                }
                _ => {
                    spins += 1;
                    if spins < 64 {
                        std::hint::spin_loop();
                    } else {
                        std::thread::yield_now();
                    }
                }
            }
        }
    }

    /// Wave 4c: chunk `i`'s chain did not land on `bounds[i + 1]`. Its anchor
    /// is a boundary (certified, or proven by the prefix), so the miss is the
    /// END bound's -- the case the eager walk handles by abandoning its anchors
    /// and walking sequentially. Do the same over the smallest suffix that can
    /// be: own chunks `i..j`, where `j` is the first chunk above `i` already
    /// DONE (its anchor is a boundary its own chain started from, so the
    /// merged chain must land on it; a DONE chunk may already have answered,
    /// and is never re-walked), or the end of the span; clear their bits and
    /// walk `[bounds[i], bounds[j])` as one chain. DONE for all of them on a
    /// landing, DEAD (unprovable, not retried) otherwise. Readers of any owned
    /// chunk wait on WALKING meanwhile.
    fn walk_merged_from(&self, bits: &ObjectStartBits, i: usize) -> bool {
        let _merge = self.merge.lock().unwrap_or_else(|e| e.into_inner());
        let n = self.state.len();
        let mut j = i;
        while j < n {
            let st = &self.state[j];
            let mut spins = 0u32;
            let owned = loop {
                let s = st.load(Ordering::Acquire);
                match s {
                    LAZY_WALKING => {
                        spins += 1;
                        if spins < 64 {
                            std::hint::spin_loop();
                        } else {
                            std::thread::yield_now();
                        }
                    }
                    // Chunk `i` itself: another thread's merge finished it.
                    LAZY_DONE if j == i => return true,
                    LAZY_DEAD if j == i => return false,
                    LAZY_DONE => break false,
                    _ => {
                        if st
                            .compare_exchange(s, LAZY_WALKING, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                        {
                            break true;
                        }
                    }
                }
            };
            if !owned {
                break;
            }
            j += 1;
        }
        /// Publishes `outcome` (DEAD on unwind) for every owned chunk.
        struct Publish<'a> {
            state: &'a [std::sync::atomic::AtomicU8],
            outcome: u8,
        }
        impl Drop for Publish<'_> {
            fn drop(&mut self) {
                for st in self.state {
                    st.store(self.outcome, Ordering::Release);
                }
            }
        }
        let mut publish = Publish {
            state: &self.state[i..j],
            outcome: LAZY_DEAD,
        };
        let (lo, hi) = (self.bounds[i], self.bounds[j]);
        bits.clear_offsets(lo, hi);
        self.merged_walks.fetch_add(1, Ordering::Relaxed);
        let ok = (self.walker)(bits.base, lo, hi, &self.skips, bits);
        if ok {
            self.bytes_walked.fetch_add(hi - lo, Ordering::Relaxed);
            publish.outcome = LAZY_DONE;
        } else {
            bits.clear_offsets(lo, hi);
        }
        drop(publish);
        ok
    }
}

/// `n` zero-valued `AtomicU64`s from `alloc_zeroed`, as a `Vec`.
///
/// See [`ObjectStartBits::new`] for why this is not `collect()`.
fn zeroed_atomic_words(n: usize) -> Vec<AtomicU64> {
    if n == 0 {
        return Vec::new();
    }
    let layout =
        std::alloc::Layout::array::<AtomicU64>(n).expect("object-start bitmap layout overflow");
    // SAFETY: `n > 0`, so `layout` is non-zero-sized.
    let ptr = unsafe { std::alloc::alloc_zeroed(layout) } as *mut AtomicU64;
    if ptr.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    // SAFETY: `ptr` was allocated by the global allocator with
    // `Layout::array::<AtomicU64>(n)` — exactly the layout a
    // `Vec<AtomicU64>` of capacity `n` owns and will deallocate with — so the
    // size, alignment and allocator all match; it is non-null (checked above);
    // and all `n` elements are initialised, because an all-zero bit pattern is
    // a valid `AtomicU64` (value 0).
    unsafe { Vec::from_raw_parts(ptr, n, n) }
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
/// gcj yg3-residuals: did the operator fix the worker count
/// (`CRATONVM_GC_PAR_THREADS`)? An explicit count is honoured as given.
pub fn young_gc_threads_configured() -> bool {
    configured_threads().is_some()
}

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
            .min(16)
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
pub fn young_gc_threads(used: usize) -> usize {
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
    /// `idle` count that can never be reached, `std::thread::scope` (today
    /// the pool's `EvacPool::scope` barrier) waits for
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

/// [`SPILL_WHEN_PEER_IDLE`] under `CRATONVM_GC_YOUNG_MARK_SHARE_EARLY` (gci
/// i2/st, opt-in): the evacuator's measured `SHARE_MIN` (`gen_evac.rs`, 8 on a
/// 4..16 plateau, 8-vs-64 a disjoint separation on bt18 with helper scans
/// halving at 64).
///
/// At 64 a depth-first mark of a TREE never shares: popping a node and pushing
/// its children grows the local stack by (fan-out - 1) per level, so a binary
/// tree of a million objects peaks near 20 entries and a linked list near 1,
/// and the worker that took the root marks the whole structure while every
/// peer stays parked (`st2_a_tree_mark_reaches_a_peer_only_with_early_sharing`
/// reproduces it). javac's ASTs and `List` cons cells are exactly those
/// shapes. Below 64 the cost is more hand-offs (a lock and a notify per spill),
/// which is why it is a switch with a cost row and not the default.
const SPILL_WHEN_PEER_IDLE_EARLY: usize = 8;

// gcg g4/yg: the spin-before-park idle policy of the two parallel young
// drains (gcf f2/y `IdleSpin`, `CRATONVM_GC_PAR_EVAC_SPIN`, default off) is
// deleted: triage tg1 (`t_ev_SPN_*` against `t_ev_A_*`) measured no gain. An
// idle worker parks on the condvar at once, as before f2/y.

/// Drain `seed` with `threads` workers, calling `scan(addr, &mut worklist)`
/// for each address. `scan` pushes newly-claimed addresses onto the worklist
/// it is handed.
///
/// With `threads <= 1` this is a plain sequential loop on the calling thread
/// (no threads are spawned), so the single-worker path is identical in
/// behaviour to the pre-parallel collector.
///
/// This form spawns `threads - 1` fresh scoped threads; the collector itself
/// calls [`drain_parallel_on`] with the heap's persistent pool.
#[cfg_attr(not(test), allow(dead_code))]
pub fn drain_parallel<F>(seed: Vec<usize>, threads: usize, scan: F)
where
    F: Fn(usize, &mut Vec<usize>) + Sync,
{
    drain_parallel_on(seed, threads, None, scan)
}

/// How many workers a parallel phase of `threads` workers really gets: the
/// calling thread plus `threads - 1` helpers, clamped to what `pool` can
/// supply when one is given (a refused OS thread leaves the pool narrower,
/// `EvacPool::ensure_helpers`). Without a pool, `threads` itself.
///
/// gce e1/y: a phase whose termination counts its workers (the mark drain's
/// `live`) must size that count from THIS number, never from the request, or
/// a narrower pool would leave the survivors waiting for a worker that never
/// starts.
fn phase_width(threads: usize, pool: Option<&crate::evac_pool::EvacPool>) -> usize {
    match pool {
        Some(p) if threads > 1 => {
            p.ensure_helpers(threads - 1);
            (threads - 1).min(p.helpers()) + 1
        }
        _ => threads.max(1),
    }
}

/// Run `worker` once on each of `width` workers: the calling thread plus
/// `width - 1` helpers, taken from `pool` (persistent threads) or, without
/// one, spawned as scoped threads. Returns only when every worker has
/// finished; a helper's panic is re-raised on the caller (both
/// `EvacPool::scope` and `std::thread::scope` guarantee that). `width` must
/// come from [`phase_width`] with the same pool.
///
/// gce e1/y: the young mark drain and the span zeroing used to open a
/// `std::thread::scope` and spawn `width - 1` fresh OS threads on EVERY call
/// -- up to five mark drains and one zeroing per non-moving young cycle, which
/// is every young cycle of a JIT-warm program. The chunked walks moved to the
/// pool in gen r4/sweep (`gen_heap::run_chunk_workers`) for exactly this cost;
/// these two were left behind.
///
/// # Nesting
///
/// `EvacPool::scope` holds the pool's dispatch mutex for its whole extent, so
/// this must never run inside another dispatch on the same pool. Every caller
/// runs on the collecting thread, between dispatches, and no worker touches
/// the pool.
fn run_phase_workers<W: Fn() + Sync>(
    width: usize,
    pool: Option<&crate::evac_pool::EvacPool>,
    worker: &W,
) {
    if width <= 1 {
        worker();
        return;
    }
    match pool {
        Some(p) => {
            let body = |_helper: usize| worker();
            // `&W` is itself `Fn()`, hence a valid `FnOnce()` driver.
            p.scope(width - 1, &body, worker);
        }
        None => std::thread::scope(|s| {
            for _ in 1..width {
                s.spawn(worker);
            }
            worker();
        }),
    }
}

/// [`drain_parallel`] on the heap's persistent worker pool when `pool` is
/// given (see [`run_phase_workers`]). The worker count is
/// [`phase_width`]`(threads, pool)`, which the census records.
pub fn drain_parallel_on<F>(
    seed: Vec<usize>,
    threads: usize,
    pool: Option<&crate::evac_pool::EvacPool>,
    scan: F,
) where
    F: Fn(usize, &mut Vec<usize>) + Sync,
{
    // gci i2/st: opt-in early sharing (`CRATONVM_GC_YOUNG_MARK_SHARE_EARLY`,
    // default OFF). Read once per drain: at most a handful per young cycle.
    let spill_when_peer_idle = if young_mark_share_early_enabled() {
        SPILL_WHEN_PEER_IDLE_EARLY
    } else {
        SPILL_WHEN_PEER_IDLE
    };
    drain_parallel_on_with(seed, threads, pool, spill_when_peer_idle, scan)
}

/// `CRATONVM_GC_YOUNG_MARK_SHARE_EARLY` (gci i2/st, default OFF): share local
/// mark work with a parked peer from [`SPILL_WHEN_PEER_IDLE_EARLY`] entries
/// instead of [`SPILL_WHEN_PEER_IDLE`]. See the former for why.
fn young_mark_share_early_enabled() -> bool {
    false}

/// [`drain_parallel_on`] with the peer-idle spill threshold given explicitly
/// (the switch's reader above, and the tests, which must not set process
/// environment variables).
fn drain_parallel_on_with<F>(
    seed: Vec<usize>,
    threads: usize,
    pool: Option<&crate::evac_pool::EvacPool>,
    spill_when_peer_idle: usize,
    scan: F,
) where
    F: Fn(usize, &mut Vec<usize>) + Sync,
{
    // gce e1/y: the width the pool can really supply; `live` below counts it.
    let threads = phase_width(threads, pool);
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

    // gci i2/st: the per-drain line under `CRATONVM_DBG_GCPHASE` (the flag the
    // sweep's own `[gcphase]` phase lines read), so a long young pause says
    // whether its mark ran wide or on one worker. Off: one load per drain and
    // a register increment per scanned object, nothing else.
    let diag = crate::gc_flags().dbg_gcphase;
    let seed_len = seed.len();
    let scanned_total = AtomicUsize::new(0);
    let scanned_max = AtomicUsize::new(0);
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
        // Declared after `_exit`, so it is folded before `live` drops.
        let mut tally = DrainTally {
            scanned: 0,
            on: diag,
            total: &scanned_total,
            max: &scanned_max,
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
                        // gcf f2/y: a FAIR share, as `ParEvac::run_worker`
                        // takes since gen r4 (its DAG measurement): taking
                        // `ACQUIRE_CHUNK` outright let the first worker to wake
                        // swallow a root seed smaller than the chunk -- every
                        // young mark starts from one shared seed -- and leave
                        // its peers to wait for an early spill. The marked set
                        // is the same either way (the closure is confluent).
                        let share = len.div_ceil(g.live.max(1));
                        let take = share.clamp(1, ACQUIRE_CHUNK).min(len);
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
                tally.scanned += 1;
                let n = local.len();
                // Publish surplus at the high-water mark as before, and also as
                // soon as a peer is parked -- see `SPILL_WHEN_PEER_IDLE`.
                if n >= SPILL_HIGH
                    || (n >= spill_when_peer_idle && idle_hint.load(Ordering::Relaxed) > 0)
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

    run_phase_workers(threads, pool, &worker);
    if diag {
        // Every worker has returned (the pool's barrier), so the tallies are
        // complete. `max_worker` near `scanned` means one worker did the mark.
        eprintln!(
            "[gcphase] drain: workers={threads} seed={seed_len} scanned={} max_worker={} ms={}",
            scanned_total.load(Ordering::Relaxed),
            scanned_max.load(Ordering::Relaxed),
            __t0.elapsed().as_millis(),
        );
    }
}

/// Per-worker scan count for the `[gcphase] drain:` line (gci i2/st), folded
/// into the drain's totals when the worker leaves by any path.
struct DrainTally<'a> {
    scanned: usize,
    on: bool,
    total: &'a AtomicUsize,
    max: &'a AtomicUsize,
}

impl Drop for DrainTally<'_> {
    fn drop(&mut self) {
        if self.on {
            self.total.fetch_add(self.scanned, Ordering::Relaxed);
            self.max.fetch_max(self.scanned, Ordering::Relaxed);
        }
    }
}

// ---------------------------------------------------------------------------
// Parallel span zeroing
// ---------------------------------------------------------------------------

/// Below this many bytes in total, [`zero_spans_parallel`] zeroes on the
/// calling thread: a memset of a megabyte costs about what spawning the
/// workers does (gen r5w3/evac7).
const ZERO_SPANS_PARALLEL_MIN_BYTES: usize = 1 << 20;

/// Zero `spans` (`(offset, size)` relative to `base`) using `threads` workers.
///
/// The spans are the sweep's reclaimed regions: pairwise disjoint and sorted,
/// so the workers touch strictly separate memory and need no synchronisation.
///
/// # Safety
/// Every `(off, sz)` must lie inside the live from-space region at `base`, and
/// the spans must be pairwise disjoint.
#[cfg_attr(not(test), allow(dead_code))]
pub unsafe fn zero_spans_parallel(base: usize, spans: &[(usize, usize)], threads: usize) {
    // SAFETY: forwarded verbatim; the caller upholds the contract above.
    unsafe { zero_spans_parallel_on(base, spans, threads, None) }
}

/// [`zero_spans_parallel`] with its helpers taken from `pool`, the heap's
/// persistent worker pool, when one is given (gce e1/y: the sweep used to
/// spawn `threads - 1` fresh OS threads for this on every young cycle; see
/// [`run_phase_workers`]). The set of bytes zeroed does not depend on the
/// pool or on the width it grants.
///
/// # Safety
/// As [`zero_spans_parallel`]; and the caller is not inside a dispatch of
/// `pool`.
pub unsafe fn zero_spans_parallel_on(
    base: usize,
    spans: &[(usize, usize)],
    threads: usize,
    pool: Option<&crate::evac_pool::EvacPool>,
) {
    let zero_one = |&(off, sz): &(usize, usize)| {
        // SAFETY: caller guarantees `[base+off, base+off+sz)` lies inside the
        // live from-space region and that the spans are pairwise disjoint.
        unsafe { std::ptr::write_bytes((base + off) as *mut u8, 0, sz) };
    };
    // Split by BYTES, not by span count: one coalesced span can be orders of
    // magnitude larger than its neighbours.
    let total: usize = spans.iter().map(|&(_, sz)| sz).sum();
    // gen r5w3/evac7: the serial cut-off is by BYTES too. It used to be
    // `spans.len() < threads`, which zeroed a FEW HUGE spans on one thread —
    // exactly the shape a pinned in-place young cycle leaves (the evacuated
    // bytes between a handful of pinned objects coalesce into one or two
    // spans of tens of megabytes; limit L4 of
    // `gengc-r4w5-pinned5-in-place-copy-limits-CLOSED-20261012.md`) — while spawning
    // `threads - 1` threads to zero a few kilobytes of many small spans.
    if threads <= 1 || total < ZERO_SPANS_PARALLEL_MIN_BYTES {
        spans.iter().for_each(zero_one);
        return;
    }
    // gce e1/y: what the pool can supply (it may be narrower than asked).
    let threads = phase_width(threads, pool);
    if threads <= 1 {
        spans.iter().for_each(zero_one);
        return;
    }
    let per = total / threads + 1;
    // gen r5w3/evac7: and a span larger than one worker's share is cut into
    // share-sized pieces, so the slicing below can hand them to different
    // workers; before, a single span was never split, and one span holding
    // most of the bytes was zeroed by one thread whatever `threads` said. The
    // pieces are disjoint, ascending sub-ranges of disjoint spans, so the set
    // of bytes zeroed is exactly the same.
    let mut pieces: Vec<(usize, usize)> = Vec::with_capacity(spans.len() + threads);
    for &(off, sz) in spans {
        let (mut o, mut left) = (off, sz);
        while left > per {
            pieces.push((o, per));
            o += per;
            left -= per;
        }
        if left > 0 {
            pieces.push((o, left));
        }
    }
    let mut slices: Vec<&[(usize, usize)]> = Vec::with_capacity(threads);
    let mut start = 0usize;
    let mut acc = 0usize;
    for i in 0..pieces.len() {
        acc += pieces[i].1;
        if acc >= per && slices.len() + 1 < threads {
            slices.push(&pieces[start..=i]);
            start = i + 1;
            acc = 0;
        }
    }
    if start < pieces.len() {
        slices.push(&pieces[start..]);
    }
    match pool {
        Some(p) if slices.len() > 1 => {
            // gci i2/st: every worker, driver included, CLAIMS slices until
            // none is left. This used to be "helper `i` zeroes slice `i + 1`",
            // which zeroed every slice only because `scope` dispatched exactly
            // `slices.len() - 1` helpers -- true while the pool never narrows
            // below `phase_width`'s answer, but a clamp in `scope` would have
            // left slices UNZEROED (handed to the allocator as zeroed free
            // blocks) with no hang and no report. Claiming is correct at any
            // dispatched width; the set of bytes zeroed is unchanged.
            let next = AtomicUsize::new(0);
            let claim = || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                match slices.get(i) {
                    Some(chunk) => chunk.iter().for_each(zero_one),
                    None => return,
                }
            };
            let body = |_helper: usize| claim();
            // `&F` is itself `Fn()`, hence a valid `FnOnce()` driver.
            p.scope(slices.len() - 1, &body, &claim);
        }
        _ => std::thread::scope(|s| {
            for chunk in slices.iter().skip(1) {
                s.spawn(move || chunk.iter().for_each(zero_one));
            }
            if let Some(first) = slices.first() {
                first.iter().for_each(zero_one);
            }
        }),
    }
}

#[cfg(test)]
mod tests;

/// JIT round 23 wave 4, lane L4: the lazy object-start grid
/// (`CRATONVM_GEN_LAZY_YOUNG_OBJECT_STARTS`).
#[cfg(test)]
mod r23_w4_lazy_start_tests;

/// JIT round 23 wave 9, lane L3 (`CRATONVM_GEN_YOUNG_MARK_SUMMARY`): the
/// summary-guided `collect_marked` returns exactly the full scan's vector.
#[cfg(test)]
mod r23_w9_mark_summary_tests;
