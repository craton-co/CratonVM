//! zt t31/zc11: R4 of the concurrent-relocation design, **remap in the next
//! mark** (`CRATONVM_ZGC_REMAP_IN_MARK=1`, opt-in). Stage 3 of
//! `docs/feature-designs/zgc-architecture.md`; the
//! design is section 11 of
//! `docs/feature-designs/zgc-architecture.md`.
//!
//! # What changes
//!
//! A slide that keeps its region relocation cycle across the mutator interval
//! (R3, `CRATONVM_ZGC_REGION_QUARANTINE`) no longer rewrites, in its pause, the
//! colored compact and array slots that name an object the cycle moved. Such a
//! slot keeps its old word until the next load heals it (the load barrier
//! forwards through the kept cycle) or the next mark visits it (the marker
//! forwards through the kept cycle and CAS-heals the slot); the cycle is
//! released at that mark's end, as R3 releases it. HotSpot's ZGC has no
//! rewrite pass at all: its next mark remaps what the load barrier did not.
//!
//! # Why a skipped slot always reads BAD until it is healed
//!
//! Only a slot whose word carries THIS cycle's mark color is skipped. The
//! slide runs in the closing pause after the remap flip
//! (`color_flip_to_remap`, `Remapped` good), so every such word is bad to
//! every mutator load and every routed reader from the release on:
//!
//! * the markers heal every slot they visit to the good color of the moment
//!   ([`super::ZgcRealHeap::mark_slot_word`], the mark color during a
//!   concurrent mark), so a live slot that was not stored to by a mutator
//!   carries the mark color at the close;
//! * a mutator store during the mark writes the mark color too
//!   (`colored_store_word`);
//! * a slot the markers do NOT visit -- the referent hole of a `Reference`,
//!   a legacy 16-byte cell (never colored) -- keeps another color (or none)
//!   and is rewritten in the pause as before.
//!
//! A slide of a stop-the-world-marked cycle (no flip: `Remapped` stayed good)
//! skips nothing.
//!
//! # Who forwards a stale word, and why the release is sound
//!
//! * The load barrier's slow path (`ZBarrierContext::forward`) asks the kept
//!   cycle first; every mutator read and every routed VM read goes through
//!   it on a colored heap.
//! * The markers forward and heal at the slot ([`super::ZgcRealHeap::mark_slot_word`]).
//! * The SATB pre-barrier forwards the overwritten value it logs.
//! * Roots, statics, the VM's side tables and legacy cells are still
//!   rewritten in the pause through the slide's pointer map (R5's step).
//!
//! At the next mark's end every live object was visited (or allocated black,
//! holding only barrier-produced words), so no live slot names from-space;
//! the cycle and its quarantine are released there (R3's release point).
//!
//! The kept cycle is published to the hot paths through a raw pointer
//! ([`ZRemapInMarkState::cycle`]) that is set and cleared only inside a pause,
//! while the `Arc` R3 keeps owns it.
//!
//! Census (`CRATONVM_GC_STATS=1`): `[GC] zgc-remap-in-mark: rim_enabled=
//! rim_engaged_slides= rim_refused= rim_last_refusal= rim_skipped=
//! rim_healed= rim_forwarded= rim_barrier_forwarded= rim_satb_forwarded=
//! rim_gen_engaged= rim_kept_past_young=`.
//!
//! # Generational mode (zt t32, section 6a)
//!
//! A young mark visits no old slot, so a cycle a MAJOR slide kept on a
//! generational heap is released only at the next major mark end; every young
//! mark end in between keeps it (`release_evac_quarantine`), and no young slide
//! runs meanwhile. A young slide still refuses.

use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use super::region_forwarding::ZRelocationCycle;

/// `CRATONVM_ZGC_REMAP_IN_MARK` (zt t31/zc11, R4; opt-in, `=1`). Read once per
/// heap, at construction.
pub(crate) fn zgc_remap_in_mark_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_ZGC_REMAP_IN_MARK")
}

/// Why a keeping slide rewrote every slot (`rim_last_refusal=`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ZRemapInMarkRefusal {
    /// The colored barrier with the color flip is not armed.
    NoFlip = 1,
    /// The cycle was marked stop-the-world (no mark color to skip by).
    NoMarkColor = 2,
    /// Generational mode, on a YOUNG slide: a young mark does not visit the
    /// old generation. (A major slide engages since zt t32 and keeps its
    /// cycle until the next major mark end.)
    Generational = 3,
}

pub(crate) struct ZRemapInMarkState {
    enabled: AtomicBool,
    /// The kept cycle, for the hot paths; null when none is kept.
    cycle: AtomicPtr<ZRelocationCycle>,
    /// The mark color the closing pause flipped away from (`0`: none this
    /// pause); the slide skips by it.
    skip_color: AtomicU64,
    engaged_slides: AtomicUsize,
    refused: AtomicUsize,
    last_refusal: AtomicU8,
    skipped: AtomicUsize,
    healed: AtomicUsize,
    forwarded: AtomicUsize,
    barrier_forwarded: AtomicUsize,
    satb_forwarded: AtomicUsize,
    /// zt t32 (6a): generational slides that engaged, and young mark ends
    /// that kept their cycle for the next major.
    gen_engaged: AtomicUsize,
    kept_past_young: AtomicUsize,
}

/// `[GC] zgc-remap-in-mark:` figures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZRemapInMarkReport {
    pub enabled: bool,
    pub engaged_slides: usize,
    pub refused: usize,
    pub last_refusal: u8,
    pub skipped: usize,
    pub healed: usize,
    pub forwarded: usize,
    pub barrier_forwarded: usize,
    pub satb_forwarded: usize,
    pub gen_engaged: usize,
    pub kept_past_young: usize,
}

impl ZRemapInMarkState {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            cycle: AtomicPtr::new(std::ptr::null_mut()),
            skip_color: AtomicU64::new(0),
            engaged_slides: AtomicUsize::new(0),
            refused: AtomicUsize::new(0),
            last_refusal: AtomicU8::new(0),
            skipped: AtomicUsize::new(0),
            healed: AtomicUsize::new(0),
            forwarded: AtomicUsize::new(0),
            barrier_forwarded: AtomicUsize::new(0),
            satb_forwarded: AtomicUsize::new(0),
            gen_engaged: AtomicUsize::new(0),
            kept_past_young: AtomicUsize::new(0),
        }
    }

    #[inline]
    pub(crate) fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// The kept cycle's answer for `addr`, if a cycle is kept and moved it.
    /// Lock-free: the pointer is set and cleared only inside a pause, and the
    /// `Arc` R3 keeps owns the cycle meanwhile.
    #[inline]
    pub(crate) fn forwarded(&self, addr: usize) -> Option<usize> {
        let p = self.cycle.load(Ordering::Acquire);
        if p.is_null() {
            return None;
        }
        // SAFETY: published by `publish_cycle` from a live `Arc` that the
        // heap's kept-cycle slot owns until `publish_cycle(None)` runs, which
        // happens inside a pause before that `Arc` is dropped.
        unsafe { (*p).forwarded(addr) }
    }

    /// Publish (or, `None`, withdraw) the kept cycle. **Inside a pause.**
    pub(crate) fn publish_cycle(&self, cycle: Option<&std::sync::Arc<ZRelocationCycle>>) {
        let p = cycle.map_or(std::ptr::null_mut(), |c| {
            std::sync::Arc::as_ptr(c) as *mut ZRelocationCycle
        });
        self.cycle.store(p, Ordering::Release);
    }

    #[inline]
    pub(crate) fn cycle_published(&self) -> bool {
        !self.cycle.load(Ordering::Relaxed).is_null()
    }

    pub(crate) fn set_skip_color(&self, color: u64) {
        self.skip_color.store(color, Ordering::Release);
    }

    #[inline]
    pub(crate) fn skip_color(&self) -> u64 {
        self.skip_color.load(Ordering::Acquire)
    }

    pub(crate) fn note_engaged(&self) {
        self.engaged_slides.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_refused(&self, why: ZRemapInMarkRefusal) {
        self.refused.fetch_add(1, Ordering::Relaxed);
        self.last_refusal.store(why as u8, Ordering::Relaxed);
    }

    pub(crate) fn note_skipped(&self, n: usize) {
        if n != 0 {
            self.skipped.fetch_add(n, Ordering::Relaxed);
        }
    }

    pub(crate) fn note_healed(&self, forwarded: bool) {
        self.healed.fetch_add(1, Ordering::Relaxed);
        if forwarded {
            self.forwarded.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn note_barrier_forwarded(&self) {
        self.barrier_forwarded.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_satb_forwarded(&self) {
        self.satb_forwarded.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_kept_past_young(&self) {
        self.kept_past_young.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn report(&self) -> ZRemapInMarkReport {
        let l = |a: &AtomicUsize| a.load(Ordering::Relaxed);
        ZRemapInMarkReport {
            enabled: self.enabled(),
            engaged_slides: l(&self.engaged_slides),
            refused: l(&self.refused),
            last_refusal: self.last_refusal.load(Ordering::Relaxed),
            skipped: l(&self.skipped),
            healed: l(&self.healed),
            forwarded: l(&self.forwarded),
            barrier_forwarded: l(&self.barrier_forwarded),
            satb_forwarded: l(&self.satb_forwarded),
            gen_engaged: l(&self.gen_engaged),
            kept_past_young: l(&self.kept_past_young),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_enabled_for_test(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }
}

impl super::ZgcRealHeap {
    /// zt t31/zc11 (R4): the `[GC] zgc-remap-in-mark:` summary figures.
    pub fn remap_in_mark_report(&self) -> ZRemapInMarkReport {
        self.counters.rim.report()
    }

    /// zt t31/zc11 (R4): does this heap heal the slots its markers visit? The
    /// switch on and the colored barrier with the color flip armed. One
    /// relaxed load when the switch is off (every default run).
    #[inline]
    pub(super) fn remap_in_mark_heals(&self) -> bool {
        self.counters.rim.enabled() && self.load_barrier_color_flip()
    }

    /// zt t31/zc11 (R4): a marker read `decoded` out of the reference slot at
    /// `slot` (a compact field or an array element: an 8-byte word). Under
    /// [`Self::remap_in_mark_heals`], forward it through the kept cycle and
    /// heal the slot to the good color of the moment with one CAS (a lost CAS
    /// is a mutator's store, which wins); answer the address the marker
    /// traces. Otherwise `decoded`, untouched.
    #[inline]
    pub(super) fn mark_slot_word(&self, slot: *const u8, decoded: u64) -> u64 {
        if decoded == 0 || !self.remap_in_mark_heals() {
            return decoded;
        }
        self.mark_slot_word_heal(slot, decoded)
    }

    /// zt t31/zc11 (R4): the referent hole of a `Reference` the marker is
    /// visiting: healed like any visited slot, never traced (the reference
    /// processor decides its referent). A slot the slide skipped is healed
    /// only by the marker or a load, and the hole has no load in the mark.
    #[inline]
    pub(super) fn mark_slot_hole(&self, slot: *const u8) {
        if !self.remap_in_mark_heals() {
            return;
        }
        // SAFETY: as `mark_slot_word_heal`.
        let raw = unsafe { cratonvm_types::narrow_oop::read_ref_slot(slot) };
        if raw != 0 {
            self.mark_slot_word_heal(slot, raw);
        }
    }

    /// zt t31/zc11 (R4): the load barrier's forward step outside a relocating
    /// slide: `offset` (a bare heap offset) through the kept cycle, identity
    /// when none is published or it did not move the object.
    #[inline]
    pub(super) fn remap_in_mark_forward_offset(&self, offset: u64) -> u64 {
        let st = &self.counters.rim;
        if !st.cycle_published() && !self.lazy_cycle_published() {
            return offset;
        }
        let base = self.arena_base as u64;
        let Some(from) = base.checked_add(offset) else {
            return offset;
        };
        // zt t32 (R6): or the published plan, which copies the object first.
        match st
            .forwarded(from as usize)
            .or_else(|| self.lazy_forwarded(from as usize))
        {
            Some(to) => match (to as u64).checked_sub(base) {
                Some(off) => {
                    st.note_barrier_forwarded();
                    off
                }
                None => offset,
            },
            None => offset,
        }
    }

    /// zt t31/zc11 (R4): does the slide leave the bare-word slot at
    /// `slot_addr` (decoded to `raw`) stale? Only when its word carries the
    /// closing mark's color `skip` and the slide's kept cycle (the index's
    /// cycle, kept by this slide) moved `raw`.
    #[inline]
    pub(super) fn remap_in_mark_skips(
        slot_addr: u64,
        skip: u64,
        raw: usize,
        fwd: &super::relocate::ZForwardIndex,
    ) -> bool {
        // SAFETY: `slot_addr` is an 8-byte-aligned reference word inside a
        // survivor (`ZSlotObservation::slot_addr`); the world is stopped.
        let word = unsafe { (*(slot_addr as *const AtomicU64)).load(Ordering::Relaxed) };
        super::vaddr::is_colored_word(word)
            && word & super::vaddr::Z_METADATA_MASK == skip
            && fwd.cycle().is_some_and(|c| c.forwarded(raw).is_some())
    }

    #[cold]
    fn mark_slot_word_heal(&self, slot: *const u8, decoded: u64) -> u64 {
        // A colored heap has wide slots (ZGC refuses compressed oops).
        debug_assert_eq!(cratonvm_types::narrow_oop::ref_field_size(), 8);
        // SAFETY: `slot` is an 8-byte-aligned reference slot inside a live
        // object the marker is visiting (the caller just read it).
        let cell = unsafe { &*(slot as *const AtomicU64) };
        let word = cell.load(Ordering::Acquire);
        if !super::vaddr::is_colored_word(word) {
            // A plain word (an unencodable address): nothing to heal.
            return decoded;
        }
        let addr = cratonvm_types::narrow_oop::decode_colored_slot_word(word) as usize;
        // zt t32 (R6): or the published plan, which copies the object first.
        let fwd = self
            .counters
            .rim
            .forwarded(addr)
            .or_else(|| self.lazy_forwarded(addr));
        let target = fwd.unwrap_or(addr);
        let desired = self.colored_store_word(target);
        if desired != word
            && cell
                .compare_exchange(word, desired, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            self.counters.rim.note_healed(fwd.is_some());
        }
        target as u64
    }

    /// zt t31/zc11 (R4): the SATB pre-barrier's logged value, forwarded through
    /// the kept cycle (an overwritten stale word names from-space). One
    /// relaxed load when no cycle is kept.
    #[inline]
    pub(super) fn satb_forward(&self, addr: usize) -> usize {
        if !self.counters.rim.cycle_published() && !self.lazy_cycle_published() {
            return addr;
        }
        // zt t32 (R6): or the published plan, which copies the object first.
        match self
            .counters
            .rim
            .forwarded(addr)
            .or_else(|| self.lazy_forwarded(addr))
        {
            Some(to) => {
                self.counters.rim.note_satb_forwarded();
                to
            }
            None => addr,
        }
    }

    /// zt t31/zc11 (R4): the slide is about to rewrite with `keeping` (the
    /// cycle it will keep). Decide whether it skips the healed slots: the
    /// switch, the flip, a kept cycle, and a mark color recorded by this
    /// pause's remap flip. Sets (or clears) the skip color the rewrite reads.
    pub(super) fn remap_in_mark_begin_slide(&self, keeping: bool) {
        // zt t33: the slide's words are good (`end_closing_heal_color`).
        self.end_closing_heal_color_for("slide");
        let st = &self.counters.rim;
        st.set_skip_color(0);
        if !st.enabled() || !keeping {
            return;
        }
        if !self.load_barrier_color_flip() {
            st.note_refused(ZRemapInMarkRefusal::NoFlip);
            return;
        }
        let generational = self.generational_enabled();
        // zt t32 (6a): a young slide still refuses (its mark visited no old
        // slot); a major one engages and keeps its cycle for the next major.
        if generational && self.counters.rim_cycle_young.load(Ordering::Relaxed) {
            st.note_refused(ZRemapInMarkRefusal::Generational);
            return;
        }
        let color = self.counters.rim_last_mark_color.swap(0, Ordering::AcqRel);
        if color == 0 {
            st.note_refused(ZRemapInMarkRefusal::NoMarkColor);
            return;
        }
        st.note_engaged();
        if generational {
            st.gen_engaged.fetch_add(1, Ordering::Relaxed);
            self.counters
                .rim_keep_until_major
                .store(true, Ordering::Relaxed);
        }
        st.set_skip_color(color);
    }

    /// zt t31/zc11 (R4): the slide's rewrite is done.
    pub(super) fn remap_in_mark_end_slide(&self) {
        self.counters.rim.set_skip_color(0);
        self.counters
            .rim_last_mark_color
            .store(0, Ordering::Relaxed);
    }

    /// zt t31/zc11 (R4): R3 kept (`Some`) or took (`None`) the region cycle;
    /// publish it to the hot paths when the switch is on. **Inside a pause.**
    pub(super) fn remap_in_mark_publish(&self, cycle: Option<&std::sync::Arc<ZRelocationCycle>>) {
        let st = &self.counters.rim;
        if st.enabled() || st.cycle_published() {
            st.publish_cycle(cycle.filter(|_| st.enabled()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reader is opt-in.
    #[test]
    fn zt31zc11_the_switch_is_opt_in() {
        let run = |v: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_ZGC_REMAP_IN_MARK", v)],
                zgc_remap_in_mark_enabled,
            )
        };
        assert!(!run(None));
        assert!(run(Some("1")));
        assert!(!run(Some("0")));
    }

    /// With nothing kept, nothing forwards; the census counts.
    #[test]
    fn zt31zc11_no_kept_cycle_forwards_nothing() {
        let st = ZRemapInMarkState::new(true);
        assert!(!st.cycle_published());
        assert_eq!(st.forwarded(0x1000), None);
        st.note_healed(true);
        st.note_healed(false);
        st.note_skipped(3);
        st.note_refused(ZRemapInMarkRefusal::NoFlip);
        let r = st.report();
        assert_eq!((r.healed, r.forwarded, r.skipped, r.refused), (2, 1, 3, 1));
        assert_eq!(r.last_refusal, ZRemapInMarkRefusal::NoFlip as u8);
    }
}
