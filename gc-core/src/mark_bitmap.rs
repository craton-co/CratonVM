// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Mark bitmap for concurrent garbage collection.
//!
//! A bit-level bitmap covering the old generation's address space. Each bit
//! represents one potential object location at `MARK_GRANULARITY`-byte
//! intervals. Used during concurrent marking to distinguish:
//!
//! - **White** (unmarked): Potentially garbage — bit is 0.
//! - **Black** (marked): Known live — bit is 1.
//!
//! Gray objects (live but unscanned) are tracked in the mark queue, not in
//! the bitmap itself.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// `clear` census -- CRATONVM_DBG_MARKCLEAR=1
// ---------------------------------------------------------------------------

/// Calls to [`MarkBitmap::clear`], calls that actually cleared, the words they
/// cleared, and the nanoseconds they spent doing it.
///
/// # Why the four numbers and not one
///
/// The change this measures is the `any_marked` early return, and its whole
/// value is the calls it turns into nothing. A total time alone cannot see
/// that: a fast total is equally consistent with "the early return is
/// carrying almost every call" and with "the bitmaps are small". The pair
/// `calls` / `worked` is the engagement number -- `worked == calls` means the
/// early return never fired on this workload and the change bought nothing
/// here -- and `words` is what says whether the calls that DID work were
/// clearing anything worth clearing.
///
/// gengc-round2-alloc2, 2026-09-20: `words` counts the words a call actually
/// STORED INTO, which since [`MarkBitmap::clear`] became bounded to the marked
/// span is no longer the same as the bitmap's capacity. `words / calls` against
/// `MarkBitmap::region_size() / 512` is therefore the engagement number for the
/// bound, the way `worked / calls` is for the `any_marked` elision. Nobody has
/// run this census on a real workload yet; the two numbers it would settle --
/// how much of the old-gen mark bitmap a concurrent cycle actually touches, and
/// what that clear costs -- are still open.
///
/// Off by default: `Instant::now()` twice per call is cheap next to a
/// bitmap-wide store loop but not next to the early return, which is one
/// relaxed load, and instrumenting the fast path with something more expensive
/// than the fast path is how a census answers a question about itself.
static CLEAR_CALLS: AtomicU64 = AtomicU64::new(0);
static CLEAR_WORKED: AtomicU64 = AtomicU64::new(0);
static CLEAR_WORDS: AtomicU64 = AtomicU64::new(0);
static CLEAR_NANOS: AtomicU64 = AtomicU64::new(0);

#[inline]
fn clear_census_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| false)}

/// `(calls, worked, words_cleared, nanos)` -- see [`CLEAR_CALLS`].
pub fn clear_census() -> (u64, u64, u64, u64) {
    (
        CLEAR_CALLS.load(Ordering::Relaxed),
        CLEAR_WORKED.load(Ordering::Relaxed),
        CLEAR_WORDS.load(Ordering::Relaxed),
        CLEAR_NANOS.load(Ordering::Relaxed),
    )
}

/// Granularity of the mark bitmap: one bit per 8 bytes of heap.
/// This matches the minimum object alignment (8-byte aligned headers).
pub const MARK_GRANULARITY: usize = 8;

/// A concurrent mark bitmap backed by atomic u64 words.
///
/// Thread-safe: multiple marker threads can set bits concurrently using
/// atomic CAS operations. The concurrent old-gen cycle builds one per cycle
/// (`maybe_concurrent_gc_at` constructs a fresh `ConcurrentMarker`) and
/// `initial_mark` clears it before any reuse; the clear at the end of a
/// cycle's sweep is dead work on that path (gen r5w5/old9,
/// `gengc-r5w5-old9-concurrent-mark-bitmap-does-not-follow-old-gen-growth`).
/// It covers the span it was built for and never grows: an address past it
/// is never marked (see that page for why that matters under
/// `CRATONVM_GC_OLD_BORROW_YOUNG`).
pub struct MarkBitmap {
    /// The storage, the atomics, the `alloc_zeroed`, the alignment-exact
    /// `locate` and the relaxed-store `clear_all` all live in
    /// [`crate::heap_bitmap::HeapBitmap`] now. This type is the mark-bitmap
    /// VOCABULARY over it -- `try_mark` / `is_marked` / `clear` -- kept so G1's
    /// per-region bitmap and `ConcurrentMarker`'s old-gen bitmap read as they
    /// did.
    ///
    /// # What the callers gain from the swap
    ///
    /// [`crate::heap_bitmap::HeapBitmap::claim`] is `try_mark`'s exact
    /// semantics plus a plain load before the read-modify-write. That test is
    /// asked of every EDGE rather than every object, and most edges point at
    /// something already marked -- a shared graph is why marking is a traversal
    /// and not a walk. The unconditional `fetch_or` this replaces made each of
    /// those an exclusive-state acquisition of a cache line every other marking
    /// worker is also writing.
    ///
    /// It is also alignment-EXACT, which this type was not: `addr` and
    /// `addr + 4` used to land on the same bit, so an unaligned candidate
    /// silently marked its neighbour. Mark bitmaps are only ever asked about
    /// object bases, which are 8-aligned, so no live caller changes behaviour --
    /// but the direction of the difference is that a bug becomes impossible
    /// rather than merely unlikely.
    bits: crate::heap_bitmap::HeapBitmap,
    /// Has ANY bit been set since the last [`Self::clear`]?
    ///
    /// Kept HERE rather than pushed down into `HeapBitmap`, because this is the
    /// only one of the two whose clear is speculative: G1 calls `clear()` from
    /// `G1Region::reset()` for every region a cleanup frees, and a young pause
    /// marks into almost none of them. `HeapBitmap`'s other instances -- ZGC's
    /// object-start registry and its mark bits -- are cleared only when they are
    /// known to be populated, so the flag would be pure cost there and one more
    /// thing for `insert` / `set_range` / `spill` to keep in step.
    ///
    /// # What makes the early return safe, and what does NOT
    ///
    /// The requirement is that `clear` must never skip a populated bitmap: a
    /// black bit surviving from the previous cycle is a live object reaped.
    ///
    /// This doc used to state that as an ORDERING property -- "set BEFORE the
    /// claim, and that order is the safety argument" -- and it is not one.
    /// [`MarkBitmap::try_mark`] does `any_marked.store(true, Release)` and then
    /// `bits.claim(addr)`, but a `Release` store constrains what may move AFTER
    /// it, not what may move BEFORE it. Nothing in the memory model stops the
    /// claim's read-modify-write from becoming visible first. On x86 it will
    /// not; on AArch64, which this tree targets, that is a permitted execution.
    /// So a concurrent `clear` could in principle read the flag as false while
    /// another thread's bit is already published -- if a concurrent `clear`
    /// were possible at all.
    ///
    /// It is not, and THAT is the safety argument. [`MarkBitmap::clear`]
    /// demands exclusive access, so something other than the store ordering --
    /// a safepoint, or for the generational post-sweep reset the old-gen lock
    /// that every concurrent mark slice and every sweep slice runs under (the
    /// remaining marks are made inside the initial-mark and remark pauses) --
    /// supplies the happens-before edge between every mark of
    /// a cycle and the clear that follows it. The store stays `Release` and the
    /// load stays cheap: strengthening either to `SeqCst` would buy nothing
    /// that exclusion does not already give, on the marking hot path. (gc-common
    /// w3-e: this said "every caller runs inside a pause", which stopped being
    /// true for `ConcurrentMarker::finish_sweep` when the sweep was sliced; see
    /// [`MarkBitmap::clear`].)
    ///
    /// (LANE W2-C, 2026-09-20. `clear_skips_only_an_untouched_bitmap` pins the
    /// behaviour; this comment is what says why it holds.)
    any_marked: AtomicBool,
    /// Lowest bitmap word index a mark has been aimed at since the last
    /// [`Self::clear`], and one PAST the highest. `(usize::MAX, 0)` when
    /// nothing has.
    ///
    /// # Why a window instead of an epoch stamp
    ///
    /// gengc-round1-alloc proposed epoch-TAGGING the bitmap: a per-word stamp,
    /// a word whose stamp is stale reading as zero, and `clear` becoming an
    /// increment. That does not close here. [`crate::heap_bitmap::HeapBitmap`]
    /// is written concurrently -- ZGC's object-start registry from every
    /// mutator's allocation path, the mark bits from every marking worker --
    /// so "re-stamp and zero the word on first touch" is a read-modify-write
    /// two threads can enter at once, and the loser's bit is lost under the
    /// winner's zeroing store unless every write takes a per-word lock. It also
    /// puts a second load on [`crate::heap_bitmap::HeapBitmap::contains`],
    /// which is on the mutator path through `jit_checkcast`. See
    /// `docs/feature-designs/generative-gc-architecture.md`.
    ///
    /// A window delivers the same shape -- O(marked span) instead of
    /// O(capacity) -- with none of that. It is a pair of monotone atomics
    /// widened on the way IN, so a reader of the bitmap is untouched, and the
    /// clear it bounds already exists
    /// ([`crate::heap_bitmap::HeapBitmap::clear_within`]) complete with the
    /// `debug_assert_clear_outside` walk that verifies the bound on every debug
    /// run. `any_marked` is the degenerate case of the same idea and is kept
    /// because it answers in one relaxed load.
    ///
    /// # The ordering, which is the safety argument
    ///
    /// Widened BEFORE the claim, exactly as `any_marked` is armed before it:
    /// a set bit must never be observable outside the window, or `clear` would
    /// leave a black bit from the previous cycle and the object it names would
    /// be swept while live. Widened on every in-range `try_mark`, not only on
    /// the ones that newly set a bit, so a re-mark of an already-set bit cannot
    /// narrow anything either.
    mark_lo_word: AtomicUsize,
    mark_hi_word: AtomicUsize,
}

impl MarkBitmap {
    /// Create a new zeroed bitmap covering a heap region.
    pub fn new(base_addr: usize, region_size: usize) -> Self {
        Self {
            bits: crate::heap_bitmap::HeapBitmap::labelled(base_addr, region_size, "mark"),
            any_marked: AtomicBool::new(false),
            mark_lo_word: AtomicUsize::new(usize::MAX),
            mark_hi_word: AtomicUsize::new(0),
        }
    }

    /// Widen the clear window to include word `w`. See [`Self::mark_lo_word`].
    ///
    /// The plain loads are the same test-before-read-modify-write discipline
    /// [`crate::heap_bitmap::HeapBitmap::claim`] uses and for the same reason:
    /// after the first few marks of a cycle every later one falls inside the
    /// window, so the common case is two loads of a line that stays
    /// read-shared. The `fetch_min` / `fetch_max` are the arbiters and the
    /// loads only skip the races they would have won unchanged.
    #[inline]
    fn widen_clear_window(&self, w: usize) {
        if w < self.mark_lo_word.load(Ordering::Relaxed) {
            self.mark_lo_word.fetch_min(w, Ordering::Release);
        }
        // Stored EXCLUSIVE so a mark in word 0 on a fresh bitmap still widens
        // (an inclusive high would be indistinguishable from the empty state).
        if w + 1 > self.mark_hi_word.load(Ordering::Relaxed) {
            self.mark_hi_word.fetch_max(w + 1, Ordering::Release);
        }
    }

    /// Attempt to mark the bit for the given address. Returns `true` if the
    /// bit was newly set (was 0, now 1). Returns `false` if already marked.
    ///
    /// Lock-free and safe to call from multiple marker threads concurrently.
    #[inline]
    pub fn try_mark(&self, addr: usize) -> bool {
        // RANGE SCREEN FIRST, and it is not optional.
        //
        // `HeapBitmap::claim` SPILLS an address its grid cannot encode into an
        // overflow set and reports it newly claimed. That is right for an
        // object-start registry, where a base the grid cannot represent must
        // still be recorded and losing it would lose an object. It is wrong
        // here, and dangerously so: a mark bitmap is bounded to ONE region and
        // G1 asks every region's bitmap about addresses that belong to other
        // regions as a matter of course. Answering "newly marked" for one of
        // those puts a foreign address on the mark queue and grows the overflow
        // set without bound.
        //
        // `MarkBitmap`'s contract has always been "not mine -> false", and
        // `out_of_range_ignored` is the test that says so. It failed the moment
        // this type started delegating, which is exactly what it is for.
        let Some((w, _)) = self.bits.locate(addr) else {
            return false;
        };
        // Both sequenced before the claim -- see `any_marked` and
        // `mark_lo_word`. Written BEFORE the claim, which reads as an ordering
        // guarantee and is not one: a `Release` store orders earlier accesses,
        // not the later claim, and on AArch64 that reordering is permitted.
        // What actually makes `clear`'s early return safe is that
        // `clear` is STW, not this store -- `any_marked` carries the argument.
        // The read-first keeps the lines read-shared once the first mark of a
        // cycle has happened.
        self.widen_clear_window(w);
        if !self.any_marked.load(Ordering::Relaxed) {
            self.any_marked.store(true, Ordering::Release);
        }
        self.bits.claim(addr)
    }

    /// Check if the bit for the given address is marked.
    #[inline]
    pub fn is_marked(&self, addr: usize) -> bool {
        // Screened for the same reason `try_mark` is: `contains` consults the
        // overflow set, and this type must answer for its own region only.
        // gcj j3/og3 (O9): one `locate`, not two -- `contains` located the
        // address again; the word is read with the same `Acquire` load
        // (`HeapBitmap::word_at`), so the answer is identical (G1 included).
        match self.bits.locate(addr) {
            Some((w, mask)) => self.bits.word_at(w) & mask != 0,
            None => false,
        }
    }

    /// Clear all bits (prepare for next GC cycle).
    ///
    /// **NO CONCURRENT READER OR WRITER.** A call while anything else can
    /// `try_mark` / `is_marked` this bitmap would erase marks out from under a
    /// running marker, which is a use-after-free rather than a torn read -- so
    /// the ordering is not what protects it, the caller's exclusion is.
    /// `HeapBitmap::clear_all` documents the same contract and the measurement
    /// behind its relaxed stores.
    ///
    /// Corrected by gc-common w3-e (2026-09-23): this used to say "every caller
    /// runs inside a pause" and list `abort_cycle` among them. `abort_cycle`
    /// no longer clears the bitmap, and the generational post-sweep reset
    /// (`ConcurrentMarker::finish_sweep`) has run OUTSIDE any pause since the
    /// concurrent sweep was sliced (gen r4w3/oldgen3): it is exclusive because
    /// every concurrent access to the marker's bitmap (mark slices, sweep
    /// slices) happens under the old-gen lock it holds, and nothing else in the
    /// tree touches that bitmap -- not because the world is stopped. The
    /// callers are `ConcurrentMarker::initial_mark` (STW) and `finish_sweep`
    /// (driver-exclusive, concurrent with mutators). G1's
    /// `start_concurrent_mark` (under the regions write lock), `G1Region::reset`
    /// (`&mut` region) and `region.rs`'s `mark_live_objects` (`&mut self`)
    /// called this too until gc-common w4-e moved all three to
    /// [`Self::clear_exclusive`], the non-atomic bulk clear
    /// (`common-g-proposal-bitmap-bulk-clear`). That form needs `&mut`, which
    /// is why these two callers, sharing the bitmap through an `Arc`, and the
    /// `finish_sweep` arm in particular, where a `StopTheWorldToken` does not
    /// exist either, stay here. (Caller list re-verified by gc-common w7-e.)
    ///
    /// The early return is the one thing this adds: see [`Self::any_marked`].
    ///
    /// # Bounded to the marked span (gengc-round2-alloc2, 2026-09-20)
    ///
    /// `any_marked` elides the whole sweep for a bitmap nothing marked into,
    /// which is what G1's per-region bitmaps mostly are. It never fires for a
    /// bitmap that IS marked into every cycle -- `ConcurrentMarker`'s covers
    /// the whole old generation -- and there the sweep was
    /// `old_gen_size / 512` bytes of stores whether the marker found ten live
    /// objects or ten million. That is the same O(capacity)-not-O(live) shape
    /// `Arena::clear_object_starts_over_bumped` removed from the object-start
    /// registry, still in place here.
    ///
    /// [`Self::mark_lo_word`] records where the marks actually went, so the
    /// sweep now covers `[lo, hi)` words and nothing else. The bound is a
    /// SUPERSET of the set bits by construction (the window is widened on
    /// every in-range `try_mark`, before the claim), and
    /// `debug_assert_clear_outside` inside `clear_within` re-proves that on
    /// every debug run rather than trusting it.
    pub fn clear(&self) {
        let census = clear_census_on();
        if census {
            CLEAR_CALLS.fetch_add(1, Ordering::Relaxed);
        }
        if !self.any_marked.load(Ordering::Acquire) {
            return;
        }
        let t0 = census.then(std::time::Instant::now);
        let lo = self.mark_lo_word.load(Ordering::Acquire);
        let hi = self.mark_hi_word.load(Ordering::Acquire);
        let words = if lo == 0 && hi >= self.bits.word_count() {
            // The window is the whole bitmap, so the bound buys nothing and
            // `clear_all` is the cheaper way to say it. Worth the branch for a
            // second reason: `clear_within` opens with
            // `debug_assert_clear_outside`, which walks the WHOLE bitmap in
            // debug builds. Routing the marked-everywhere case here keeps that
            // verification walk off the one shape where it can find nothing --
            // there are no words outside the range to check.
            self.bits.clear_all();
            self.bits.word_count()
        } else if lo < hi {
            // Word indices back to arena addresses. `word_base(w)` is
            // `base + w * 512` and `word_range` recovers exactly `(lo, hi)`
            // from that pair, clamped to the covered span, so the conversion
            // is lossless rather than a widening.
            self.bits
                .clear_within(&[(self.bits.word_base(lo), self.bits.word_base(hi))]);
            hi - lo
        } else {
            // `any_marked` is armed but the window is empty. Unreachable
            // today -- `try_mark` widens the window before it arms the flag,
            // and it is the only writer -- so this is the conservative arm for
            // a future writer that arms one and not the other. Clearing
            // everything is always correct; skipping is not.
            self.bits.clear_all();
            self.bits.word_count()
        };
        self.mark_lo_word.store(usize::MAX, Ordering::Relaxed);
        self.mark_hi_word.store(0, Ordering::Relaxed);
        self.any_marked.store(false, Ordering::Relaxed);
        if let Some(t0) = t0 {
            CLEAR_WORKED.fetch_add(1, Ordering::Relaxed);
            // The words this call actually stored into, NOT `word_count()`.
            // The whole question the census exists to answer is how much of
            // the bitmap a clear touches, and reporting the capacity would
            // make the bound it now applies invisible to its own instrument.
            CLEAR_WORDS.fetch_add(words as u64, Ordering::Relaxed);
            CLEAR_NANOS.fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
    }

    /// [`Self::clear`] for an owner holding the bitmap EXCLUSIVELY (`&mut`):
    /// the same untouched-bitmap skip and marked-word window, with the words
    /// zeroed by `memset` ([`crate::heap_bitmap::HeapBitmap::clear_all_exclusive`] /
    /// `clear_within_exclusive`) instead of one relaxed store per word.
    ///
    /// gc-common w3-e, the implementable half of
    /// `common-g-proposal-bitmap-bulk-clear`. Its callers (since gc-common
    /// w4-e) are the three that already hold `&mut`: `G1Region::reset` (every
    /// region a cleanup frees), `start_concurrent_mark`'s per-region loop under
    /// the regions write lock, and `region.rs`'s `mark_live_objects`;
    /// `ConcurrentMarker`'s bitmap is shared through an `Arc` and stays on
    /// [`Self::clear`]. Counted in the
    /// same `CRATONVM_DBG_MARKCLEAR` census, so an A/B compares like with like.
    pub fn clear_exclusive(&mut self) {
        let census = clear_census_on();
        if census {
            CLEAR_CALLS.fetch_add(1, Ordering::Relaxed);
        }
        if !*self.any_marked.get_mut() {
            return;
        }
        let t0 = census.then(std::time::Instant::now);
        let lo = *self.mark_lo_word.get_mut();
        let hi = *self.mark_hi_word.get_mut();
        let words = if lo < hi && !(lo == 0 && hi >= self.bits.word_count()) {
            let (a, b) = (self.bits.word_base(lo), self.bits.word_base(hi));
            self.bits.clear_within_exclusive(&[(a, b)]);
            hi - lo
        } else {
            // Whole-bitmap window, or the armed-but-empty window `clear`
            // also answers conservatively: clear everything.
            self.bits.clear_all_exclusive();
            self.bits.word_count()
        };
        *self.mark_lo_word.get_mut() = usize::MAX;
        *self.mark_hi_word.get_mut() = 0;
        *self.any_marked.get_mut() = false;
        if let Some(t0) = t0 {
            CLEAR_WORKED.fetch_add(1, Ordering::Relaxed);
            CLEAR_WORDS.fetch_add(words as u64, Ordering::Relaxed);
            CLEAR_NANOS.fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
    }

    /// Has anything been marked into this bitmap since the last [`Self::clear`]?
    #[inline]
    pub fn any_marked(&self) -> bool {
        self.any_marked.load(Ordering::Acquire)
    }

    /// Count the total number of marked bits (for statistics).
    ///
    /// # LANE W6-A — why this is NOT wired to a shutdown report
    ///
    /// `w3c-instrumentation-audit.md` §6 grouped this with
    /// `base_addr`/`region_size` as "read only by `impl Debug`, and nothing
    /// formats a `MarkBitmap`". Those two were deleted by W4-B; this one is
    /// kept, and the reason is not the `Debug` impl — that formatter still
    /// never runs, and "read by a `Debug` impl" is still not read.
    ///
    /// It is kept because it has twenty-one real callers in
    /// `concurrent_mark.rs`'s and this file's unit tests, where it is the ONLY
    /// statement of what a mark actually reached: `a_zero_budget_slice…` and
    /// the sliced-vs-whole equivalence test both assert on it and have no
    /// other way to say the thing they are testing.
    ///
    /// It is not wired to `print_gc_summary` or `collector_decision_report`
    /// because **the object it reads is empty by the time either one runs**.
    /// A bitmap's bits are a transient of one mark cycle: [`Self::clear`] wipes
    /// them at the end of it and `G1Region::reset` wipes the per-region one on
    /// recycle, so a shutdown reader gets a structural zero on every run that
    /// completed its last cycle and a meaningless partial count on one that did
    /// not. Wiring it would publish a number that is zero for a reason having
    /// nothing to do with the heap — which is the `all-zero census` defect
    /// `w3c-instrumentation-audit.md` §3 names, one level up.
    ///
    /// The number a shipped binary WOULD want — how many objects the last
    /// cycle marked — has to be accumulated at the moment [`Self::clear`] runs,
    /// beside `CLEAR_WORDS`. That is a change to how the figure is COMPUTED,
    /// not a new reader for an existing one, so W6-A filed it rather than made
    /// it: see `w6a-twenty-nine-accessors.md` §5.
    pub fn marked_count(&self) -> usize {
        (0..self.bits.word_count())
            .map(|w| self.bits.word_at(w).count_ones() as usize)
            .sum()
    }
}

impl std::fmt::Debug for MarkBitmap {
    // LANE W4-B — the two fields below used to be `pub fn base_addr()` and
    // `pub fn region_size()`, and this impl was their ONLY reader. They are
    // read from `self.bits` directly now.
    //
    // "Read by a `Debug` impl" is not read: nothing in the workspace formats a
    // `MarkBitmap` (none of its three containers derives `Debug`), so the two
    // accessors were public API whose whole justification was a formatter that
    // never runs. They were also not counters — they are structural getters,
    // and there is no question about a RUN that either one answers, which is
    // the test this sweep applied. The impl itself stays: it costs nothing and
    // a container that derives `Debug` later must not fail to compile.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MarkBitmap")
            .field("base_addr", &format_args!("{:#x}", self.bits.base))
            .field("region_size", &self.bits.span)
            .field("num_words", &self.bits.word_count())
            .field("marked", &self.marked_count())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
