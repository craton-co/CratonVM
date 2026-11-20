use super::*;

/// An instance row's guard word round-trips its exact class (`0`: none),
/// and no other guard word -- no guard, an intrinsic's receiver class, the
/// four `0xFFFF_FFFx` markers -- decodes as one.
#[test]
fn instance_guard_words_round_trip_and_never_alias_another_guard() {
    for exact in [0u32, 1, 42, 0x7FFF_FFEF] {
        let word = sync_direct_instance_guard(exact).expect("encodable");
        assert_eq!(sync_direct_instance_guard_class(word), Some(exact));
    }
    for other in [
        0u32,
        1,
        42,
        0x7FFF_FFFF,
        JIT_CYCLE_EDGE_CELL_GUARD,
        JIT_RETIRE_CELL_GUARD,
        JIT_SELF_CALL_GUARD,
        JIT_SYNC_DIRECT_GUARD,
        u32::MAX,
    ] {
        assert_eq!(sync_direct_instance_guard_class(other), None, "{other:#x}");
    }
    // Ids that would collide with the tag or the markers are refused.
    assert_eq!(sync_direct_instance_guard(0x7FFF_FFF0), None);
    assert_eq!(
        sync_direct_instance_guard(JIT_SYNC_DIRECT_INSTANCE_TAG),
        None
    );
}
