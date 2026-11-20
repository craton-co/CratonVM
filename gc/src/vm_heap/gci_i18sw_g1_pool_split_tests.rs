use super::g1_pool_committed_split;

const MB: usize = 1024 * 1024;

/// Right after a pause: no eden region, two survivors, 10.5 MiB old in a
/// 64 MiB committed heap with a 20-region young target. HotSpot: survivor
/// 2 regions, old 11 regions plus the remainder, eden 18 regions.
#[test]
fn gci_i18sw_eden_committed_is_the_young_target_less_the_survivors() {
    let (eden, surv, old) = g1_pool_committed_split(64 * MB, MB, 0, 2, 10 * MB + MB / 2, 20);
    assert_eq!(surv, 2 * MB);
    assert_eq!(eden, 18 * MB);
    assert_eq!(old, 64 * MB - 20 * MB);
    assert_eq!(eden + surv + old, 64 * MB, "the split partitions the committed heap");
}

/// The young target is capped by what is left after survivor and old.
#[test]
fn gci_i18sw_eden_takes_at_most_what_is_left() {
    let (eden, surv, old) = g1_pool_committed_split(16 * MB, MB, 3, 1, 9 * MB, 40);
    assert_eq!(surv, MB);
    assert_eq!(old, 9 * MB);
    assert_eq!(eden, 6 * MB);
    assert_eq!(eden + surv + old, 16 * MB);
}

/// An eden that ran past a lowered target keeps at least its regions; a
/// committed figure below the census never underflows.
#[test]
fn gci_i18sw_eden_never_below_its_regions_and_no_underflow() {
    let (eden, _, _) = g1_pool_committed_split(64 * MB, MB, 12, 2, 4 * MB, 5);
    assert_eq!(eden, 12 * MB);
    let (eden, surv, old) = g1_pool_committed_split(4 * MB, MB, 2, 2, 7 * MB, 10);
    assert_eq!((eden, surv, old), (0, 2 * MB, 7 * MB));
}

/// gci i19 (vi18-sw U `sw18_cu1_g1`): after a `System.gc()` that left
/// the live data young, a 38-region target in a 16 MiB committed heap
/// gives eden at most 60 % of the COMMITTED regions (9) less the
/// survivors, so old keeps a positive committed size, as on HotSpot.
#[test]
fn gci_i19_old_keeps_the_committed_remainder_after_a_young_system_gc() {
    let (eden, surv, old) = g1_pool_committed_split(16 * MB, MB, 0, 1, 0, 38);
    assert_eq!(surv, MB);
    assert_eq!(eden, 8 * MB);
    assert_eq!(old, 7 * MB);
    assert!(old > 0);
}
