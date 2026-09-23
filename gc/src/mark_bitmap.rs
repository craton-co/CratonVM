// SPDX-License-Identifier: Apache-2.0
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
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_MARKCLEAR").is_some())
}

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
/// atomic CAS operations. The bitmap is allocated once per GC cycle and
/// cleared between cycles.
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
    /// It is not, and THAT is the safety argument. [`MarkBitmap::clear`] is
    /// documented STOP-THE-WORLD ONLY and every caller runs inside a pause, so
    /// the safepoint -- not the store ordering -- supplies the happens-before
    /// edge between every mark of a cycle and the clear that follows it. The
    /// store stays `Release` and the load stays cheap: strengthening either to
    /// `SeqCst` would buy nothing the safepoint does not already give, on the
    /// marking hot path.
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
    /// `docs/internal/reviews/gengc-round2-alloc2-20260920.md`.
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
        // guarantee and is not one: a `Release` store orders what follows it,
        // not the claim that precedes it, and on AArch64 that reordering is
        // permitted. What actually makes `clear`'s early return safe is that
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
        self.bits.locate(addr).is_some() && self.bits.contains(addr)
    }

    /// Clear all bits (prepare for next GC cycle).
    ///
    /// **STOP-THE-WORLD ONLY.** Every caller runs inside a pause:
    /// `ConcurrentMarker::initial_mark` / `abort_cycle` / the post-sweep reset,
    /// and `G1Region::reset` from G1's cleanup. A call with the world alive
    /// would erase marks out from under a running marker, which is a
    /// use-after-free rather than a torn read -- so the ordering is not what
    /// protects it, the caller's safepoint is. `HeapBitmap::clear_all` documents
    /// the same contract and the measurement behind its relaxed stores.
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
mod tests {
    use super::*;

    #[test]
    fn new_bitmap_all_clear() {
        let bm = MarkBitmap::new(0x1000, 4096);
        assert_eq!(bm.marked_count(), 0);
        assert!(!bm.is_marked(0x1000));
    }

    #[test]
    fn try_mark_returns_true_then_false() {
        let bm = MarkBitmap::new(0x0, 1024);
        // First mark: should succeed
        assert!(bm.try_mark(0));
        assert!(bm.is_marked(0));
        // Second mark: already set
        assert!(!bm.try_mark(0));
        assert_eq!(bm.marked_count(), 1);
    }

    #[test]
    fn mark_multiple_addresses() {
        let bm = MarkBitmap::new(0x0, 4096);
        assert!(bm.try_mark(0));
        assert!(bm.try_mark(8));
        assert!(bm.try_mark(16));
        assert!(bm.try_mark(4088)); // near end
        assert_eq!(bm.marked_count(), 4);
    }

    #[test]
    fn out_of_range_ignored() {
        let bm = MarkBitmap::new(0x1000, 1024);
        assert!(!bm.try_mark(0x0)); // below base
        assert!(!bm.try_mark(0x2000)); // above end
        assert!(!bm.is_marked(0x0));
    }

    #[test]
    fn clear_resets_all() {
        let bm = MarkBitmap::new(0x0, 1024);
        bm.try_mark(0);
        bm.try_mark(64);
        bm.try_mark(512);
        assert_eq!(bm.marked_count(), 3);
        bm.clear();
        assert_eq!(bm.marked_count(), 0);
        assert!(!bm.is_marked(0));
    }

    /// A cleared bitmap must read as empty from another thread.
    ///
    /// `clear()` is relaxed stores plus one trailing `Release` fence (it used
    /// to be an `AcqRel` swap per word; see its doc for the measurement that
    /// changed it). The property that has to hold is unchanged and is what is
    /// asserted: after a clear, no reader observes a bit from before it. The
    /// join below is the happens-before edge in this test, standing in for the
    /// stop-the-world pause exit that provides it in production.
    #[test]
    fn clear_publishes_zeroes_to_marker() {
        use std::sync::Arc;
        let bm = Arc::new(MarkBitmap::new(0x0, 4096));
        // Mark many bits, then clear in a thread, and read in another —
        // every reader observation post-clear must be `false`.
        for i in 0..512 {
            bm.try_mark(i * 8);
        }
        assert!(bm.marked_count() > 0);
        let bm_clearer = bm.clone();
        let clearer = std::thread::spawn(move || {
            bm_clearer.clear();
        });
        clearer.join().unwrap();
        // After clear+join (which provides happens-before), every bit
        // must be observed as unmarked.
        for i in 0..512 {
            assert!(!bm.is_marked(i * 8), "stale mark at {i}");
        }
        assert_eq!(bm.marked_count(), 0);
    }

    /// `clear()` may skip its sweep ONLY for a bitmap nothing has marked into.
    ///
    /// The direction that matters is the unsafe one: a bitmap with a bit set
    /// must never take the early return, because the bit would survive into the
    /// next cycle as a stale black mark and the object it names would be
    /// treated as live-then-reaped. `try_mark` sets `any_marked` BEFORE its
    /// `fetch_or` precisely so that ordering cannot invert.
    #[test]
    fn clear_skips_only_an_untouched_bitmap() {
        let bm = MarkBitmap::new(0x1000, 4096);
        assert!(!bm.any_marked(), "a fresh bitmap has nothing to clear");
        bm.clear();
        assert!(!bm.any_marked());

        // A mark that LANDED arms the flag...
        assert!(bm.try_mark(0x1000));
        assert!(bm.any_marked());
        bm.clear();
        assert!(!bm.any_marked(), "clear disarms");
        assert!(!bm.is_marked(0x1000), "and actually cleared the bit");

        // ...and a re-mark of an ALREADY-set bit must keep it armed, or the
        // second clear of a cycle would skip a populated bitmap.
        assert!(bm.try_mark(0x1008));
        assert!(!bm.try_mark(0x1008), "second mark is a no-op on the bit");
        assert!(bm.any_marked(), "but must not disarm the flag");
        bm.clear();
        assert!(!bm.is_marked(0x1008));
    }

    /// An out-of-range mark must not arm the flag into claiming work that
    /// cannot exist -- and, more importantly, must not be reported as newly
    /// marked.
    #[test]
    fn out_of_range_mark_does_not_arm_the_flag() {
        let bm = MarkBitmap::new(0x1000, 1024);
        assert!(!bm.try_mark(0x0));
        assert!(!bm.try_mark(0x9000));
        assert!(!bm.any_marked());
    }

    /// The constructor hands back a genuinely zeroed bitmap.
    ///
    /// It builds one from `vec![0u64; n]` and reinterprets the allocation as
    /// `[AtomicU64]` rather than constructing each atomic; this pins the bit
    /// pattern that conversion assumes.
    #[test]
    fn alloc_zeroed_bitmap_reads_as_empty() {
        let bm = MarkBitmap::new(0x1_0000, 1 << 20);
        assert_eq!(bm.marked_count(), 0);
        for i in (0..(1usize << 20)).step_by(4096) {
            assert!(!bm.is_marked(0x1_0000 + i), "stale bit at +{i:#x}");
        }
    }

    /// An address outside this bitmap's region must not be recorded ANYWHERE.
    ///
    /// The companion to `out_of_range_ignored`, and the reason it is a separate
    /// test: that one checks the return value, this one checks that nothing was
    /// stored. `HeapBitmap::claim` spills an unencodable address into an
    /// overflow set — correct for an object-start registry, where losing such a
    /// base loses an object, and wrong for a bounded mark bitmap that G1 asks
    /// about other regions' addresses as a matter of course. Without the range
    /// screen in `try_mark` the overflow set would grow without bound and a
    /// foreign address would be reported newly marked.
    #[test]
    fn an_out_of_region_mark_does_not_reach_the_overflow_set() {
        let bm = MarkBitmap::new(0x1_0000, 4096);
        assert!(!bm.try_mark(0x0), "below the region");
        assert!(!bm.try_mark(0x9_0000), "above the region");
        assert!(!bm.try_mark(0x1_0004), "inside, but not on the 8-byte grid");
        assert!(
            !bm.bits.has_spill(),
            "a mark bitmap is bounded to one region; nothing outside it may be              recorded, and an overflow entry is both a wrong answer and an              unbounded leak"
        );
        assert_eq!(bm.marked_count(), 0);
        assert!(!bm.any_marked());
    }

    /// gengc-round2-alloc2, 2026-09-20 — the clear is bounded to the span the
    /// marks landed in, and every bit in that span still goes.
    ///
    /// The direction that matters is the unsafe one, and it is the same one
    /// `clear_skips_only_an_untouched_bitmap` guards from the other side: a
    /// bit OUTSIDE the window would survive the clear as a stale black mark.
    /// So the test marks at both extremes of a large bitmap — the pair that
    /// makes a window computed from only one of them wrong — plus one in the
    /// middle, and re-reads the whole bitmap afterwards. In a debug build
    /// `clear_within`'s own `debug_assert_clear_outside` walk is the second,
    /// stronger check: it asserts every word outside the bound was already
    /// zero before the stores.
    #[test]
    fn clear_is_bounded_to_the_marked_span_and_still_clears_all_of_it() {
        const BASE: usize = 0x10_0000;
        const SPAN: usize = 1 << 20; // 2048 bitmap words
        let bm = MarkBitmap::new(BASE, SPAN);
        assert!(bm.bits.word_count() > 64, "need a bitmap worth bounding");

        // First and last grid slots, and one in between: a window derived from
        // any single mark would leave one of the other two behind.
        let marks = [BASE, BASE + SPAN / 2, BASE + SPAN - 8];
        for m in marks {
            assert!(bm.try_mark(m), "{m:#x} must be in range and newly marked");
        }
        assert_eq!(bm.marked_count(), 3);
        bm.clear();
        for m in marks {
            assert!(!bm.is_marked(m), "stale mark survived at {m:#x}");
        }
        assert_eq!(bm.marked_count(), 0);
        assert!(!bm.any_marked());

        // The window must RESET with the clear, or the next cycle's bound
        // would be the union of every cycle so far and grow back to the
        // capacity it exists to avoid. It is also where the NARROWING is
        // observable: one word out of 2048, against a `clear_all` that would
        // store into all of them.
        assert_eq!(bm.mark_lo_word.load(Ordering::Relaxed), usize::MAX);
        assert_eq!(bm.mark_hi_word.load(Ordering::Relaxed), 0);
        assert!(bm.try_mark(BASE + 512));
        let (lo, hi) = (
            bm.mark_lo_word.load(Ordering::Relaxed),
            bm.mark_hi_word.load(Ordering::Relaxed),
        );
        // One bitmap word covers 512 ARENA bytes (64 bits x 8), so offset 512
        // is bit 64, i.e. word 1 — and the window is half-open.
        assert_eq!((lo, hi), (1, 2));
        assert!(hi - lo < bm.bits.word_count() / 64, "the bound must bite");
        bm.clear();
        assert!(!bm.is_marked(BASE + 512));
    }

    /// A mark the range screen rejects must not widen the window either.
    ///
    /// The companion to `out_of_range_mark_does_not_arm_the_flag`: an
    /// out-of-region address is one G1 asks every region's bitmap about as a
    /// matter of course, and letting one widen the window would restore the
    /// full-capacity clear on the first foreign probe of a cycle.
    #[test]
    fn an_out_of_range_mark_does_not_widen_the_clear_window() {
        let bm = MarkBitmap::new(0x1000, 4096);
        assert!(!bm.try_mark(0x0));
        assert!(!bm.try_mark(0x9000));
        assert!(!bm.try_mark(0x1004), "inside, but off the 8-byte grid");
        assert_eq!(bm.mark_lo_word.load(Ordering::Relaxed), usize::MAX);
        assert_eq!(bm.mark_hi_word.load(Ordering::Relaxed), 0);
    }

    /// A RE-mark of an already-set bit must keep the window as wide as the
    /// first one made it.
    ///
    /// `try_mark` returns `false` for a bit that is already set, and a window
    /// widened only on the newly-set path would be correct only because the
    /// first mark already widened it. Widening before the claim — on every
    /// in-range call — is what makes that independent of `claim`'s answer, and
    /// this is the test that says so.
    #[test]
    fn a_repeat_mark_cannot_narrow_the_window() {
        let bm = MarkBitmap::new(0x0, 1 << 16);
        assert!(bm.try_mark(0x8000));
        let before = (
            bm.mark_lo_word.load(Ordering::Relaxed),
            bm.mark_hi_word.load(Ordering::Relaxed),
        );
        assert!(!bm.try_mark(0x8000), "second mark is a no-op on the bit");
        assert_eq!(
            (
                bm.mark_lo_word.load(Ordering::Relaxed),
                bm.mark_hi_word.load(Ordering::Relaxed),
            ),
            before,
        );
        bm.clear();
        assert!(!bm.is_marked(0x8000));
    }

    #[test]
    fn concurrent_marking() {
        use std::sync::Arc;
        let bm = Arc::new(MarkBitmap::new(0x0, 8192));
        let mut handles = Vec::new();

        for t in 0..4 {
            let bm = bm.clone();
            handles.push(std::thread::spawn(move || {
                let mut newly_marked = 0usize;
                for i in 0..256 {
                    let addr = ((t * 256 + i) * MARK_GRANULARITY) % 8192;
                    if bm.try_mark(addr) {
                        newly_marked += 1;
                    }
                }
                newly_marked
            }));
        }

        let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        // Each address should be marked exactly once (no overlap in ranges)
        assert_eq!(total, bm.marked_count());
    }
}
