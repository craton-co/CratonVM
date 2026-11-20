use super::*;

/// Only the optimizing tier's zero-divisor guard exit is counted: the
/// plain resume point AT one of the four integer divisions. Any other
/// reason, any other opcode, or an unreadable bci is left alone.
#[test]
pub fn only_a_transfer_at_an_integer_division_is_a_division_guard_trap() {
    use cratonvm_jit::deopt::DeoptReason as R;
    for op in [0x6c_u8, 0x6d, 0x70, 0x71] {
        assert!(
            is_division_guard_trap(R::TransferToInterpreter, Some(op)),
            "{op:#x}"
        );
    }
    // fdiv, ddiv, frem, drem never trap; iadd is not a division.
    for op in [0x6e_u8, 0x6f, 0x72, 0x73, 0x60] {
        assert!(
            !is_division_guard_trap(R::TransferToInterpreter, Some(op)),
            "{op:#x}"
        );
    }
    assert!(!is_division_guard_trap(R::TransferToInterpreter, None));
    for reason in [
        R::BoundsCheck,
        R::UncommonTrap,
        R::SpeculationFailed,
        R::PendingException,
    ] {
        assert!(!is_division_guard_trap(reason, Some(0x6c)), "{reason:?}");
    }
}

/// The decision fires on the exit that reaches the threshold; later exits
/// (a long-running caller's baked CALL) re-fire it only at powers of two
/// (round 21, REVVM221-1), other sites count on their own, and a full
/// table is cleared rather than grown.
#[test]
pub fn division_guard_counter_fires_once_at_the_threshold() {
    let mut counts: rustc_hash::FxHashMap<u32, u32> = rustc_hash::FxHashMap::default();
    for i in 1..4 {
        assert!(!note_division_guard_trap(&mut counts, 7, 4, 16), "exit {i}");
    }
    assert!(
        note_division_guard_trap(&mut counts, 7, 4, 16),
        "the 4th decides"
    );
    for i in 5..=14 {
        assert_eq!(
            note_division_guard_trap(&mut counts, 7, 4, 16),
            i == 8,
            "exit {i}: only a power of two re-fires"
        );
    }
    assert!(!note_division_guard_trap(&mut counts, 8, 4, 16));

    let mut small: rustc_hash::FxHashMap<u32, u32> = rustc_hash::FxHashMap::default();
    let _ = note_division_guard_trap(&mut small, 1, 4, 2);
    let _ = note_division_guard_trap(&mut small, 2, 4, 2);
    let _ = note_division_guard_trap(&mut small, 3, 4, 2);
    assert_eq!(small.len(), 1, "a full table is cleared before a new key");
}


/// A non-division trap never touches the counter or the tier memo, so the
/// existing per-bci de-spec policy is the only one that applies to it.
#[test]
pub fn a_non_division_trap_is_not_retiered() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    for _ in 0..(DIVISION_GUARD_RETIER_THRESHOLD * 2) {
        retier_hot_division_guard_trap(
            &shared,
            "R9w10Exc10NotADivision",
            "f",
            "(II)I",
            cratonvm_types::ClassId::new(0),
            cratonvm_jit::deopt::DeoptReason::TransferToInterpreter,
            Some(0x60),
            2,
        );
    }
    let h = cratonvm_jit::ir_method_memo_hash("R9w10Exc10NotADivision", "f", "(II)I");
    let key: DivisionGuardSiteKey = (
        shared.vm_identity,
        h,
        0,
        shared.jit.jit_cache.redefine_epoch(),
        2,
    );
    assert!(
        DIVISION_GUARD_TRAPS.with(|t| t.borrow().get(&key).is_none()),
        "an iadd bci is never counted"
    );
}

/// Interpreter round i1 wave 23, lane L5
/// (`i22-L5-ir-site-trap-decisions-are-process-wide-by-method-name`): two
/// VMs running the same method each re-tier their own copy, and a
/// redefined version re-tiers again. Until then the decision was a
/// process static keyed by the method's names: the second VM's claim
/// failed and its optimizing body stayed installed, exiting on every zero
/// divisor for the life of the process.
#[test]
pub fn each_vm_and_each_class_version_re_tiers_its_own_hot_division_site() {
    const C: &str = "L5W23PerVmDivisionRetier";
    let a = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let b = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let id = cratonvm_types::ClassId::new(0);
    let h = cratonvm_jit::ir_method_memo_hash(C, "div", "(II)I");
    let exits = |vm: &crate::vm::SharedVm| {
        for _ in 0..DIVISION_GUARD_RETIER_THRESHOLD {
            retier_hot_division_guard_trap(
                vm,
                C,
                "div",
                "(II)I",
                id,
                cratonvm_jit::deopt::DeoptReason::TransferToInterpreter,
                Some(0x6c),
                2,
            );
        }
    };
    let decided = |vm: &crate::vm::SharedVm| {
        // A claim that SUCCEEDS here means the re-tier did not take it.
        !crate::runtime::interpreter::jit_bridge::jit_verdicts(vm)
            .claim_site_trap_decision(h, id)
    };
    exits(&a);
    assert!(decided(&a), "VM A re-tiers its copy");
    exits(&b);
    assert!(
        decided(&b),
        "VM B re-tiers its own copy: VM A's decision is not VM B's"
    );
    a.jit
        .note_class_redefinition_of(cratonvm_types::ClassId::new(0x2317), C, false);
    exits(&a);
    assert!(decided(&a), "the redefined version re-tiers again");
}

pub fn local_handler_site(
    candidates: Vec<(&'static str, u32)>,
    declaring_class_id: ClassId,
) -> cratonvm_jit::JitLocalHandlerSite {
    cratonvm_jit::JitLocalHandlerSite {
        candidates,
        declaring_class_id: declaring_class_id.as_u32(),
        throw_bci: 0,
        cache: std::sync::atomic::AtomicU64::new(
            cratonvm_jit::JitLocalHandlerSite::CACHE_EMPTY,
        ),
        implicit_cache: cratonvm_jit::JitLocalHandlerSite::empty_implicit_cache(),
        no_collect: false,
    }
}

/// i1-L5 catch-type page, wave 4: a candidate whose catch type will not
/// load DECLINES (`-1`, propagate to the interpreter-side route, which
/// throws the resolution error) instead of being skipped. Before, the
/// catch-all behind it was entered with the original exception.
#[test]
pub fn local_handler_lookup_declines_on_an_unloadable_catch_type() {
    const EXC: &str = "cratonvm/test/LocalHandlerSlowExc";
    const MISSING: &str = "cratonvm/test/LocalHandlerSlowMissingCatchType";
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let exc = shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class(EXC, 0)
        .expect("Compatible mode fabricates");

    let site = local_handler_site(vec![(MISSING, 10), ("", 20)], exc);
    assert_eq!(local_handler_index_slow(&shared, &site, exc), -1);

    // Rows before the failing one are still answered: a catch-all first,
    // or an exact catch type first, takes it without reaching the row.
    let site = local_handler_site(vec![("", 10), (MISSING, 20)], exc);
    assert_eq!(local_handler_index_slow(&shared, &site, exc), 0);
    let site = local_handler_site(vec![(EXC, 10), (MISSING, 20)], exc);
    assert_eq!(local_handler_index_slow(&shared, &site, exc), 0);
}

/// Wave 6: a declaring class with its own loader namespace never resolves
/// an unloaded catch type through the loader-blind `load_class_concurrent`
/// (which could pick a same-named class-path copy and skip the row); it
/// declines with the uncached sentinel, and the catch-all behind the row is
/// not entered.
#[test]
pub fn local_handler_lookup_declines_uncached_for_a_user_loader_declaring_class() {
    const EXC: &str = "cratonvm/test/LocalHandlerForkExc";
    const DECLARING: &str = "cratonvm/test/LocalHandlerForkOwner";
    const UNLOADED: &str = "cratonvm/test/LocalHandlerForkUnloadedCatchType";
    use cratonvm_types::ClassLoaderId;
    const FORK: ClassLoaderId = ClassLoaderId::UserDefined(4343);
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let (exc, declaring) = {
        let mut cm = shared.classes.class_manager.write();
        let exc = cm
            .try_ensure_synthetic_class(EXC, 0)
            .expect("Compatible mode fabricates");
        let declaring = cm
            .try_ensure_synthetic_class(DECLARING, 0)
            .expect("Compatible mode fabricates");
        cm.class_store
            .get_mut(declaring)
            .expect("just fabricated")
            .loader_id = FORK;
        cm.register_class_name(FORK, DECLARING, declaring);
        (exc, declaring)
    };
    let site = local_handler_site(vec![(UNLOADED, 10), ("", 20)], declaring);
    assert_eq!(
        local_handler_index_slow(&shared, &site, exc),
        LOCAL_HANDLER_DECLINED
    );
    // A catch-all first is still answered without reaching the row.
    let site = local_handler_site(vec![("", 10), (UNLOADED, 20)], declaring);
    assert_eq!(local_handler_index_slow(&shared, &site, exc), 0);
}
