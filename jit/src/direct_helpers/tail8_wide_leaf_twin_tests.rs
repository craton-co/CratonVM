use super::*;

unsafe extern "C" fn twins(ctx: i64, which: i64) -> i64 {
    match (ctx, which) {
        (0x1000, WELL_KNOWN_LEAF_TWIN_CURRENT_THREAD) => 0x7100,
        (0x1000, WELL_KNOWN_LEAF_TWIN_CHECK_INDEX) => 0x7200,
        (0x1000, WELL_KNOWN_LEAF_TWIN_THREAD_LOCAL_GET) => 0x7300,
        (0x1000, WELL_KNOWN_LEAF_TWIN_UNSAFE_CAS_INT) => 0x7400,
        (0x1000, WELL_KNOWN_LEAF_TWIN_UNSAFE_CAS_LONG) => 0x7500,
        (0x1000, WELL_KNOWN_LEAF_TWIN_VECTORIZED_HASH_CODE) => 0x7600,
        _ => 0,
    }
}

fn table() -> DirectHelperTable {
    DirectHelperTable {
        reachability_fence: 0x100,
        thread_current_thread: 0x200,
        preconditions_check_index: 0x300,
        thread_local_get: 0x500,
        unsafe_cas_int: 0x600,
        unsafe_cas_long: 0x700,
        vectorized_hash_code: 0x800,
        // Reviewed and refused: must stay `MayReachVm` under both switches.
        hashmap_get: 0x900,
        thread_local_set: 0xA00,
        integer_int_value: 0xB00,
        object_get_class: 0xC00,
        unsafe_get_int: 0xD00,
        well_known_class_id: twins as *const () as usize,
        static_base_resolver_ctx: 0x1000,
        ..DirectHelperTable::EMPTY
    }
}

#[test]
fn the_wide_rows_carry_a_decline_their_helper_never_returns() {
    let t = table();
    let twin = |entry| t.thin_helper_vm_reach_with(entry, false, true);
    assert_eq!(
        twin(0x500),
        ThinHelperVmReach::LeafTwin {
            twin: 0x7300,
            decline: -1
        }
    );
    assert_eq!(
        twin(0x600),
        ThinHelperVmReach::LeafTwin {
            twin: 0x7400,
            decline: -1
        }
    );
    assert_eq!(
        twin(0x700),
        ThinHelperVmReach::LeafTwin {
            twin: 0x7500,
            decline: -1
        }
    );
    assert_eq!(
        twin(0x800),
        ThinHelperVmReach::LeafTwin {
            twin: 0x7600,
            decline: LEAF_TWIN_DECLINE_OUTSIDE_INT
        }
    );
    // The hash helper answers every `int` sign-extended: the decline must
    // be outside that range.
    assert!(
        LEAF_TWIN_DECLINE_OUTSIDE_INT < i64::from(i32::MIN)
            || LEAF_TWIN_DECLINE_OUTSIDE_INT > i64::from(i32::MAX)
    );
}

#[test]
fn each_switch_admits_only_its_own_rows() {
    let t = table();
    for entry in [0x500, 0x600, 0x700, 0x800] {
        assert_eq!(
            t.thin_helper_vm_reach_with(entry, true, false),
            ThinHelperVmReach::MayReachVm,
            "{entry:#x} is a wide row"
        );
        // `thin_helper_vm_reach` is the narrow set, unchanged.
        assert_eq!(t.thin_helper_vm_reach(entry), ThinHelperVmReach::MayReachVm);
    }
    for entry in [0x100, 0x200, 0x300] {
        assert_eq!(
            t.thin_helper_vm_reach_with(entry, false, true),
            ThinHelperVmReach::MayReachVm,
            "{entry:#x} is a narrow row"
        );
    }
    // No wide row is ever jumped to.
    for entry in [0x500, 0x600, 0x700, 0x800] {
        assert!(!t
            .thin_helper_vm_reach_with(entry, true, true)
            .never_reaches_vm());
    }
}

#[test]
fn the_reviewed_refusals_stay_may_reach_vm() {
    let t = table();
    for entry in [0x900, 0xA00, 0xB00, 0xC00, 0xD00] {
        assert_eq!(
            t.thin_helper_vm_reach_with(entry, true, true),
            ThinHelperVmReach::MayReachVm,
            "{entry:#x}"
        );
    }
}

#[test]
fn an_unpublished_wide_twin_keeps_the_call() {
    let t = DirectHelperTable {
        static_base_resolver_ctx: 0,
        ..table()
    };
    for entry in [0x500, 0x600, 0x700, 0x800] {
        assert_eq!(
            t.thin_helper_vm_reach_with(entry, true, true),
            ThinHelperVmReach::MayReachVm
        );
    }
}
