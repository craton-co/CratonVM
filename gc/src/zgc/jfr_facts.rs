// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! zr z4/jf: the facts one ZGC collection leaves behind for the VM's JFR
//! writers (`vm/src/runtime/interpreter/gc_events.rs`), stashed inside the
//! pause and taken once by the collection's event finish.
//!
//! The `gc` crate does not depend on either JFR backend, so the collector only
//! records what it measured; the VM decides whether a recording runs and how
//! the facts become events (`jdk.GCPhasePauseLevel1`, `jdk.GCPhaseConcurrent`,
//! `jdk.ZRelocationSet`, `jdk.ZRelocationSetGroup`, `jdk.ZUncommit`).
//!
//! # Honesty rules (`zgc-zr1tr-proposal-zgc-jfr-phase-and-stall-events-DONE-20261012.md`)
//!
//! * The sub-phase rows are this collector's OWN laps (`ZCycleLaps`), under
//!   names that say what the work is. They are all inside one stop-the-world
//!   pause, so none of them is reported as a HotSpot concurrent phase.
//! * `Concurrent Mark` is reported by the VM only for a cycle whose tracing
//!   really ran off the safepoint (`marked_concurrently`), over the wall span
//!   between the mark-start pause and this collection's pause.
//! * A relocation set is reported only for a cycle that ran the selector.
//!
//! Every write is one uncontended lock per collection, whether or not a
//! recording runs (the collector cannot know); nothing is allocated unless a
//! lap was timed.

use parking_lot::Mutex;

use super::forwarding::{
    PageCandidate, ZRelocationSet, ZFWD_SIZE_CLASS_LARGE, ZFWD_SIZE_CLASS_MEDIUM,
    ZFWD_SIZE_CLASS_SMALL,
};

/// One `jdk.ZRelocationSetGroup` row: the candidates of one page size class
/// and what the selector took from them. Field meanings follow HotSpot's
/// event (`candidatePages`, `total`, `empty`, `selectedPages`, `relocate`);
/// `total` / `empty` are the candidates' ALLOCATED extent
/// (`PageCandidate::capacity_bytes`), not the page span, because bytes above a
/// page's cursor were never allocated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZJfrRelocationGroup {
    /// HotSpot's `ZPageTypeType` text: `Small`, `Medium` or `Large`.
    pub page_type: &'static str,
    pub candidate_pages: u64,
    pub total: u64,
    pub empty: u64,
    pub selected_pages: u64,
    pub relocate: u64,
}

/// One `jdk.ZRelocationSet` row plus its per-class groups.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZJfrRelocationSet {
    /// Allocated extent of every candidate page.
    pub total: u64,
    /// Allocated extent of the candidates with no live byte.
    pub empty: u64,
    /// Live bytes the selected pages hand to the slide.
    pub relocate: u64,
    pub groups: Vec<ZJfrRelocationGroup>,
}

impl ZJfrRelocationSet {
    /// Summarise one selection.
    pub fn from_selection(candidates: &[PageCandidate], selected: &ZRelocationSet) -> Self {
        let classes = [
            (ZFWD_SIZE_CLASS_SMALL, "Small"),
            (ZFWD_SIZE_CLASS_MEDIUM, "Medium"),
            (ZFWD_SIZE_CLASS_LARGE, "Large"),
        ];
        let mut groups: Vec<ZJfrRelocationGroup> = classes
            .iter()
            .map(|&(_, page_type)| ZJfrRelocationGroup {
                page_type,
                ..Default::default()
            })
            .collect();
        let slot = |class: u8| classes.iter().position(|&(c, _)| c == class);
        let (mut total, mut empty) = (0u64, 0u64);
        for c in candidates {
            let cap = c.capacity_bytes as u64;
            total = total.saturating_add(cap);
            let is_empty = c.live_bytes == 0;
            if is_empty {
                empty = empty.saturating_add(cap);
            }
            if let Some(i) = slot(c.size_class_index) {
                let g = &mut groups[i];
                g.candidate_pages += 1;
                g.total = g.total.saturating_add(cap);
                if is_empty {
                    g.empty = g.empty.saturating_add(cap);
                }
            }
        }
        for p in selected.pages() {
            if let Some(i) = slot(p.size_class_index) {
                let g = &mut groups[i];
                g.selected_pages += 1;
                g.relocate = g.relocate.saturating_add(p.live_bytes as u64);
            }
        }
        Self {
            total,
            empty,
            relocate: selected.total_live_bytes() as u64,
            groups,
        }
    }
}

/// The facts of the last collection, as [`ZJfrFacts::take`] hands them over.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZJfrCycle {
    /// The pause's sub-phases in order, `(name, microseconds)`. Empty when no
    /// lap was timed (no pause clock ran).
    pub marks: Vec<(&'static str, u128)>,
    /// The cycle's tracing ran off the safepoint (it closed a concurrent mark).
    pub marked_concurrently: bool,
    pub relocation_set: Option<ZJfrRelocationSet>,
    /// `(bytes, nanoseconds)` given back to the OS inside this pause, when the
    /// uncommit step timed itself ([`ZJfrFacts::note_uncommit`]).
    pub uncommit: Option<(u64, u64)>,
    /// zs s7/jf: `jdk.GCReferenceStatistics` -- the references this
    /// collection's reference pass found with an unreachable referent, in
    /// HotSpot's order (soft, weak, final, phantom). Stated by the VM's
    /// reference pass ([`ZJfrFacts::note_reference_counts`]); `None` when it
    /// did not run.
    pub reference_counts: Option<[u64; 4]>,
}

impl ZJfrCycle {
    fn is_empty(&self) -> bool {
        self.marks.is_empty()
            && !self.marked_concurrently
            && self.relocation_set.is_none()
            && self.uncommit.is_none()
            && self.reference_counts.is_none()
    }
}

/// HotSpot 25 ZGC's small page size and object-size granule (`ZGranuleSize`).
const HOTSPOT_ZGC_SMALL_PAGE: u64 = 2 * 1024 * 1024;
/// HotSpot 25 ZGC's medium page ceiling (16 granules).
const HOTSPOT_ZGC_MEDIUM_PAGE_MAX: u64 = 32 * 1024 * 1024;

/// zs s7/jf: the page HotSpot 25 ZGC asks for to serve an allocation of
/// `object_bytes` on a heap of `max_heap_bytes` -- the `type` and `size` of
/// its `jdk.ZAllocationStall` (the event names the page allocation that
/// stalled, not the object). HotSpot's arithmetic (`ZHeuristics::
/// set_medium_page_size`, `ZObjectAllocator`):
///
/// * small page: 2 MiB, for objects up to 1/8 of it (256 KiB);
/// * medium page: `MaxHeapSize / 32` clamped to `[2 MiB, 32 MiB]` and
///   rounded down to a power of two, enabled only when that is above the
///   small page (so `-Xmx64m` has none: HotSpot's `Medium Page Size: N/A`),
///   for objects up to 1/8 of it;
/// * large page: the object rounded up to the 2 MiB granule.
///
/// This VM allocates small objects from TLAB chunks of one arena, not from
/// pages: the request whose refusal stalled is real, the page it maps to is
/// HotSpot's (`zgc-zr4jf-allocation-stall-event-has-no-size-FIXED-20261019.md`).
pub fn hotspot_stall_page(object_bytes: u64, max_heap_bytes: u64) -> (&'static str, u64) {
    let small_limit = HOTSPOT_ZGC_SMALL_PAGE / 8;
    if object_bytes <= small_limit {
        return ("Small", HOTSPOT_ZGC_SMALL_PAGE);
    }
    let clamped = (max_heap_bytes / 32).clamp(HOTSPOT_ZGC_SMALL_PAGE, HOTSPOT_ZGC_MEDIUM_PAGE_MAX);
    // Round down to a power of two (`clamped >= 2 MiB > 0`).
    let medium = 1u64 << (63 - clamped.leading_zeros());
    if medium > HOTSPOT_ZGC_SMALL_PAGE && object_bytes <= medium / 8 {
        return ("Medium", medium);
    }
    let size = object_bytes
        .div_ceil(HOTSPOT_ZGC_SMALL_PAGE)
        .saturating_mul(HOTSPOT_ZGC_SMALL_PAGE);
    ("Large", size)
}

/// The per-heap stash (one per `ZgcRealHeap`, so per VM).
#[derive(Debug)]
pub struct ZJfrFacts {
    cur: Mutex<ZJfrCycle>,
    /// zs s4/ob3: `CRATONVM_ZGC_PAUSE_PHASES`, read once per heap: the pause
    /// clock runs on every collection so its phase boundaries can be booked
    /// as HotSpot's pauses ([`crate::gc_metrics::ZgcPauseSplit`]).
    pause_split_wanted: bool,
    /// zs s4/ob3: the last collection's phase boundaries, taken once by the
    /// VM's seal (inside the same pause). Kept apart from `cur`, which only a
    /// JFR recording takes.
    pause_split: Mutex<Option<crate::gc_metrics::ZgcPauseSplit>>,
}

impl Default for ZJfrFacts {
    fn default() -> Self {
        Self::with_pause_split(crate::gc_metrics::zgc_pause_phases_enabled())
    }
}

/// The sub-phase rows of one cycle, from the collector's laps. Names describe
/// the work; every row is pause time. `mark_end_us` is the finish of a
/// concurrent mark when `marked_concurrently`, otherwise the pause's
/// preamble (root-filter rebuild, concurrent-start bookkeeping).
#[allow(clippy::too_many_arguments)]
pub fn lap_marks(
    marked_concurrently: bool,
    relocated: bool,
    mark_end_us: u128,
    tlab_us: u128,
    snapshot_us: u128,
    mark_us: u128,
    resurrect_us: u128,
    refs_us: u128,
    sweep_us: u128,
    relocate_us: u128,
) -> Vec<(&'static str, u128)> {
    let mut marks = Vec::with_capacity(8);
    marks.push((
        if marked_concurrently {
            "Mark End"
        } else {
            "Pause Preamble"
        },
        mark_end_us,
    ));
    marks.push(("Retire TLABs", tlab_us));
    marks.push(("Snapshot Object Starts", snapshot_us));
    marks.push(("Mark", mark_us));
    marks.push(("Finalizer Resurrection", resurrect_us));
    marks.push(("Reference Processing", refs_us));
    marks.push(("Sweep", sweep_us));
    if relocated {
        marks.push(("Relocate", relocate_us));
    }
    marks
}

impl ZJfrFacts {
    /// A stash with `CRATONVM_ZGC_PAUSE_PHASES` given rather than read (tests).
    pub fn with_pause_split(wanted: bool) -> Self {
        Self {
            cur: Mutex::new(ZJfrCycle::default()),
            pause_split_wanted: wanted,
            pause_split: Mutex::new(None),
        }
    }

    /// zs s4/ob3: does this heap record its pause's phase boundaries? The
    /// collection arms its pause clock when it does.
    pub fn pause_split_wanted(&self) -> bool {
        self.pause_split_wanted
    }

    /// zs s4/ob3: the collection's phase boundaries, as its pause clock
    /// measured them: where the mark start ended (the end of the object-start
    /// snapshot's lap; not used when the cycle's mark ran concurrently, its
    /// mark start having been a pause of its own) and where the mark end
    /// ended (the end of the reference-processing lap). Nothing is recorded
    /// unless wanted, or when the clock did not run.
    pub fn note_pause_split(
        &self,
        marked_concurrently: bool,
        mark_start_end: Option<std::time::Instant>,
        mark_end_end: Option<std::time::Instant>,
    ) {
        if !self.pause_split_wanted {
            return;
        }
        let split = match (marked_concurrently, mark_start_end, mark_end_end) {
            (true, _, Some(me)) => Some(crate::gc_metrics::ZgcPauseSplit::new(None, me)),
            (false, Some(ms), Some(me)) => {
                Some(crate::gc_metrics::ZgcPauseSplit::new(Some(ms), me))
            }
            _ => None,
        };
        *self.pause_split.lock() = split;
    }

    /// zs s4/ob3: take the last collection's phase boundaries, leaving none.
    pub fn take_pause_split(&self) -> Option<crate::gc_metrics::ZgcPauseSplit> {
        if !self.pause_split_wanted {
            return None;
        }
        self.pause_split.lock().take()
    }

    /// The cycle's laps (`record_metrics_for_cycle`).
    pub fn note_laps(&self, marks: Vec<(&'static str, u128)>, marked_concurrently: bool) {
        let mut c = self.cur.lock();
        c.marks = marks;
        c.marked_concurrently = marked_concurrently;
    }

    /// zt t1/jf (X1): the cycle's tracing ran off the safepoint, timed or not
    /// (an untimed collection notes no laps and lost this fact).
    pub fn note_marked_concurrently(&self, marked_concurrently: bool) {
        self.cur.lock().marked_concurrently = marked_concurrently;
    }

    /// The cycle's relocation-set selection.
    pub fn note_relocation_set(&self, set: ZJfrRelocationSet) {
        self.cur.lock().relocation_set = Some(set);
    }

    /// Bytes the pause returned to the OS and how long that took. Summed if a
    /// pause gives back twice.
    pub fn note_uncommit(&self, bytes: u64, nanos: u64) {
        if bytes == 0 {
            return;
        }
        let mut c = self.cur.lock();
        let (b, n) = c.uncommit.unwrap_or((0, 0));
        c.uncommit = Some((b.saturating_add(bytes), n.saturating_add(nanos)));
    }

    /// zs s7/jf: the reference pass's per-type counts for
    /// `jdk.GCReferenceStatistics` (soft, weak, final, phantom). Replaces an
    /// earlier note of the same collection.
    pub fn note_reference_counts(&self, counts: [u64; 4]) {
        self.cur.lock().reference_counts = Some(counts);
    }

    /// Take the last collection's facts, leaving none (`None` when the cycle
    /// recorded nothing). A collection whose facts nobody took is replaced by
    /// the next one's laps; its relocation set and uncommit are cleared by
    /// [`Self::begin_cycle`].
    pub fn take(&self) -> Option<ZJfrCycle> {
        let c = std::mem::take(&mut *self.cur.lock());
        (!c.is_empty()).then_some(c)
    }

    /// Start of a collection: forget an untaken previous cycle so its rows
    /// can never be filed under this one.
    pub fn begin_cycle(&self) {
        *self.cur.lock() = ZJfrCycle::default();
        // zs s4/ob3: nor its pause boundaries (a seal that never took them).
        if self.pause_split_wanted {
            *self.pause_split.lock() = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn zs7jf_stall_page_follows_hotspots_page_arithmetic() {
        // Small: up to 256 KiB, a 2 MiB page whatever the heap.
        assert_eq!(hotspot_stall_page(16, 64 * MIB), ("Small", 2 * MIB));
        assert_eq!(
            hotspot_stall_page(256 * 1024, 1024 * MIB),
            ("Small", 2 * MIB)
        );
        // -Xmx64m: no medium page (64M / 32 = 2M, not above the small page).
        assert_eq!(
            hotspot_stall_page(256 * 1024 + 8, 64 * MIB),
            ("Large", 2 * MIB)
        );
        assert_eq!(hotspot_stall_page(3 * MIB, 64 * MIB), ("Large", 4 * MIB));
        // -Xmx256m: medium 8 MiB (HotSpot 25.0.3: `Range [2M, 8M]`), objects
        // up to 1 MiB.
        assert_eq!(
            hotspot_stall_page(300 * 1024, 256 * MIB),
            ("Medium", 8 * MIB)
        );
        assert_eq!(hotspot_stall_page(MIB, 256 * MIB), ("Medium", 8 * MIB));
        assert_eq!(hotspot_stall_page(MIB + 8, 256 * MIB), ("Large", 2 * MIB));
        // -Xmx1g and above: medium 32 MiB (`Range [4M, 32M]`), objects up to 4 MiB.
        assert_eq!(
            hotspot_stall_page(4 * MIB, 1024 * MIB),
            ("Medium", 32 * MIB)
        );
        assert_eq!(
            hotspot_stall_page(4 * MIB, 4096 * MIB),
            ("Medium", 32 * MIB)
        );
        assert_eq!(hotspot_stall_page(5 * MIB, 4096 * MIB), ("Large", 6 * MIB));
        // A non-power-of-two heap rounds the medium page down: 384M / 32 = 12M -> 8M.
        assert_eq!(hotspot_stall_page(MIB, 384 * MIB), ("Medium", 8 * MIB));
        // No overflow on an absurd request.
        assert_eq!(hotspot_stall_page(u64::MAX, 64 * MIB).0, "Large");
    }

    #[test]
    fn zs7jf_reference_counts_are_a_fact_of_the_cycle() {
        let f = ZJfrFacts::with_pause_split(false);
        f.note_reference_counts([1, 2, 3, 4]);
        let c = f.take().expect("reference counts alone are a fact");
        assert_eq!(c.reference_counts, Some([1, 2, 3, 4]));
        assert!(f.take().is_none(), "taken once");
        f.note_reference_counts([5, 6, 7, 8]);
        f.begin_cycle();
        assert!(
            f.take().is_none(),
            "a new collection forgets the old counts"
        );
    }

    fn cand(id: u64, live: usize, cap: usize, class: u8) -> PageCandidate {
        PageCandidate {
            page_id: id,
            live_bytes: live,
            capacity_bytes: cap,
            size_class_index: class,
        }
    }

    #[test]
    fn zr4jf_relocation_set_sums_candidates_and_selection_per_class() {
        let cands = vec![
            cand(1, 0, 100, ZFWD_SIZE_CLASS_SMALL),
            cand(2, 10, 100, ZFWD_SIZE_CLASS_SMALL),
            cand(3, 90, 100, ZFWD_SIZE_CLASS_SMALL),
            cand(4, 50, 1000, ZFWD_SIZE_CLASS_MEDIUM),
        ];
        let policy = crate::zgc::forwarding::ZRelocationPolicy::default();
        let sel = ZRelocationSet::select(&cands, &policy);
        let s = ZJfrRelocationSet::from_selection(&cands, &sel);
        assert_eq!(s.total, 1300);
        assert_eq!(s.empty, 100);
        assert_eq!(s.relocate, sel.total_live_bytes() as u64);
        let small = s.groups[0];
        assert_eq!(small.page_type, "Small");
        assert_eq!(small.candidate_pages, 3);
        assert_eq!(small.total, 300);
        assert_eq!(small.empty, 100);
        let selected: u64 = s.groups.iter().map(|g| g.selected_pages).sum();
        assert_eq!(selected, sel.len() as u64);
        let relocate: u64 = s.groups.iter().map(|g| g.relocate).sum();
        assert_eq!(relocate, s.relocate);
        assert_eq!(s.groups[2].candidate_pages, 0);
    }

    #[test]
    fn zr4jf_facts_are_taken_once_and_cleared_at_cycle_start() {
        let f = ZJfrFacts::default();
        assert!(f.take().is_none());
        f.note_laps(lap_marks(false, true, 1, 2, 3, 4, 5, 6, 7, 8), false);
        f.note_uncommit(4096, 10);
        f.note_uncommit(0, 99);
        f.note_uncommit(4096, 5);
        let c = f.take().expect("facts");
        assert_eq!(c.marks.len(), 8);
        assert_eq!(c.marks[0].0, "Pause Preamble");
        assert_eq!(c.marks[7], ("Relocate", 8));
        assert_eq!(c.uncommit, Some((8192, 15)));
        assert!(f.take().is_none());
        f.note_uncommit(1, 1);
        f.begin_cycle();
        assert!(f.take().is_none());
    }

    #[test]
    fn zr4jf_a_concurrent_cycle_names_its_first_lap_mark_end_and_skips_no_relocate() {
        let m = lap_marks(true, false, 1, 2, 3, 4, 5, 6, 7, 8);
        assert_eq!(m[0], ("Mark End", 1));
        assert_eq!(m.len(), 7);
        assert!(m.iter().all(|(n, _)| !n.starts_with("Concurrent")));
    }

    /// zs s4/ob3: the pause boundaries are recorded only when wanted, taken
    /// once, cleared by the next cycle's start; a concurrently marked cycle
    /// splits in two (its mark start was its own pause), a pause-marked one in
    /// three, and an unclocked one not at all.
    #[test]
    fn zs4ob3_pause_split_is_recorded_once_and_only_when_wanted() {
        let t = std::time::Instant::now();
        let (ms, me) = (
            t + std::time::Duration::from_micros(5),
            t + std::time::Duration::from_micros(9),
        );
        let off = ZJfrFacts::with_pause_split(false);
        assert!(!off.pause_split_wanted());
        off.note_pause_split(false, Some(ms), Some(me));
        assert_eq!(off.take_pause_split(), None, "the =0 kill records nothing");

        let on = ZJfrFacts::with_pause_split(true);
        on.note_pause_split(false, Some(ms), Some(me));
        let s = on.take_pause_split().expect("recorded");
        assert_eq!((s.pauses(), s.bounds()), (3, &[ms, me][..]));
        assert_eq!(on.take_pause_split(), None, "taken once");

        on.note_pause_split(true, Some(ms), Some(me));
        assert_eq!(on.take_pause_split().map(|s| s.pauses()), Some(2));

        on.note_pause_split(false, None, None);
        assert_eq!(on.take_pause_split(), None, "no clock, no split");

        on.note_pause_split(false, Some(ms), Some(me));
        on.begin_cycle();
        assert_eq!(on.take_pause_split(), None, "never reaches the next cycle");
    }
}
