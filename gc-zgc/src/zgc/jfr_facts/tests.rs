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
