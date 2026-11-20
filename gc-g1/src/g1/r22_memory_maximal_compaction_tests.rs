use super::*;

/// JIT round 22 wave 11: the ordinary full compaction keeps its 1/8-region
/// floor; the OOME path's maximal pass takes any garbage-bearing region.
#[test]
fn r22_memory_maximal_pass_drops_the_eighth_region_floor() {
    const MIB: usize = 1024 * 1024;
    assert_eq!(full_compaction_min_garbage(MIB, false), MIB / 8);
    assert_eq!(full_compaction_min_garbage(MIB, true), 1);
    // A ~90 %-live region: refused by the ordinary pass, taken by the maximal one.
    let garbage = MIB / 10;
    assert!(garbage < full_compaction_min_garbage(MIB, false));
    assert!(garbage >= full_compaction_min_garbage(MIB, true));
}
