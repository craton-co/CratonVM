use super::*;

unsafe extern "C" fn twins(ctx: i64, which: i64) -> i64 {
    match (ctx, which) {
        (0x1000, WELL_KNOWN_LEAF_TWIN_CURRENT_THREAD) => 0x7100,
        (0x1000, WELL_KNOWN_LEAF_TWIN_CHECK_INDEX) => 0x7200,
        _ => 0,
    }
}

fn table() -> DirectHelperTable {
    DirectHelperTable {
        reachability_fence: 0x100,
        thread_current_thread: 0x200,
        preconditions_check_index: 0x300,
        integer_value_of: 0x400,
        thread_local_get: 0x500,
        well_known_class_id: twins as *const () as usize,
        static_base_resolver_ctx: 0x1000,
        ..DirectHelperTable::EMPTY
    }
}

#[test]
fn only_the_fence_may_run_without_its_callers_frame() {
    let t = table();
    assert_eq!(t.thin_helper_vm_reach(0x100), ThinHelperVmReach::Never);
    for entry in [0x200, 0x300, 0x400, 0x500, 0x999] {
        assert!(
            !t.thin_helper_vm_reach(entry).never_reaches_vm(),
            "entry {entry:#x} must keep its caller's frame"
        );
    }
}

#[test]
fn the_two_twins_carry_a_decline_their_helper_never_returns() {
    let t = table();
    assert_eq!(
        t.thin_helper_vm_reach(0x200),
        ThinHelperVmReach::LeafTwin {
            twin: 0x7100,
            decline: 0
        }
    );
    assert_eq!(
        t.thin_helper_vm_reach(0x300),
        ThinHelperVmReach::LeafTwin {
            twin: 0x7200,
            decline: -1
        }
    );
}

#[test]
fn an_unpublished_twin_or_an_unwired_slot_may_reach_the_vm() {
    let no_vm = DirectHelperTable {
        static_base_resolver_ctx: 0,
        ..table()
    };
    assert_eq!(
        no_vm.thin_helper_vm_reach(0x200),
        ThinHelperVmReach::MayReachVm
    );
    assert_eq!(
        no_vm.thin_helper_vm_reach(0x300),
        ThinHelperVmReach::MayReachVm
    );
    // The fence needs no twin.
    assert_eq!(no_vm.thin_helper_vm_reach(0x100), ThinHelperVmReach::Never);
    // `0` is "not wired" in every slot: an unwired `reachability_fence`
    // must never make some entry read as `Never`.
    let e = DirectHelperTable::EMPTY;
    assert_eq!(e.thin_helper_vm_reach(0), ThinHelperVmReach::MayReachVm);
    assert_eq!(e.thin_helper_vm_reach(0x100), ThinHelperVmReach::MayReachVm);
}
