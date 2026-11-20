use super::{
    indy_retarget_recompile_census_in, note_indy_retarget_recompile_in, JitBridgeState,
    StaticInitLog, STATIC_INIT_LOG_CAP,
};

/// `CRATONVM_JIT_STATIC_INIT_RACE_EXACT`: past the window the per-class
/// answer still names only the classes that finished after the compile
/// began, where the windowed answer gave up (and called every initialized
/// holder raced).
#[test]
fn r22_runtime_static_init_race_is_exact_past_the_window() {
    let mut log = StaticInitLog::default();
    for id in 1..=10u32 {
        log.push_tracked(id, true);
    }
    let seq0 = log.seq;
    // Cast: a small constant.
    let later = 2 * STATIC_INIT_LOG_CAP as u32;
    for id in 100..100 + later {
        log.push_tracked(id, true);
    }
    assert!(log.initialized_since(seq0).is_none(), "the window no longer reaches back");
    for id in 1..=10 {
        assert!(!log.initialized_after(id, seq0), "initialized before the compile");
    }
    assert!(log.initialized_after(100, seq0));
    assert!(log.initialized_after(99 + later, seq0));
    assert!(!log.initialized_after(7_000_000, seq0), "never logged: not raced");
    // A recompile that began after all of them sees none raced: no repeat.
    let seq1 = log.seq;
    assert!((100..100 + later).all(|id| !log.initialized_after(id, seq1)));
    // Review w2 item 2: no rows with the switch off; unloaded classes' rows go.
    let mut off = StaticInitLog::default();
    off.push_tracked(5, false);
    assert!(off.by_class.is_empty() && off.seq == 1);
    log.forget_classes(&|id| id == 100);
    assert!(!log.initialized_after(100, seq0));
    assert!(log.initialized_after(101, seq0));
    // Review w3 item 4: the window's entries are tombstoned too, its reach kept.
    let mut window = StaticInitLog::default();
    window.push_tracked(40, false);
    let before = window.seq;
    window.push_tracked(41, false);
    window.push_tracked(42, false);
    window.forget_classes(&|id| id == 41);
    assert_eq!(window.initialized_since(before), Some(vec![42]));
    assert_eq!(window.initialized_since(0), Some(vec![40, 42]));
}

#[test]
fn r22_runtime_static_init_race_exact_is_default_on() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    assert!(super::static_init_race_exact_enabled(&shared));
}

#[test]
fn r22_runtime_retarget_census_counts_both_answers_per_vm() {
    let a = JitBridgeState::default();
    let b = JitBridgeState::default();
    assert_eq!(indy_retarget_recompile_census_in(&a), (0, 0));
    note_indy_retarget_recompile_in(&a, true);
    note_indy_retarget_recompile_in(&a, true);
    note_indy_retarget_recompile_in(&a, false);
    assert_eq!(indy_retarget_recompile_census_in(&a), (2, 1));
    // Per VM: another VM's state is untouched.
    assert_eq!(indy_retarget_recompile_census_in(&b), (0, 0));
}

/// The direct call is enabled, and the counter is noted on
/// every retarget recompile request, not only under `CRATONVM_DBG_JITC`.
#[test]
fn r22_runtime_indy_leaf_direct_call_is_enabled_and_the_census_is_unconditional() {
    let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    assert!(super::indy_leaf_direct_call_enabled(&shared));
    let indy = concat!(
        include_str!("../../invokedynamic/mod.rs"),
        "\n",
        include_str!("../../invokedynamic/dbg_indy_all.rs"),
        "\n",
        include_str!("../../invokedynamic/execute_invokedynamic_at_uncensused.rs"),
        "\n",
        include_str!("../../invokedynamic/object_methods_getter_refusal.rs"),
        "\n",
        include_str!("../../invokedynamic/descriptor_param_count_and_last_is.rs")
    );
    let miss = indy
        .split("fn note_retarget_miss(")
        .nth(1)
        .expect("note_retarget_miss");
    let note = miss
        .find("note_indy_retarget_recompile(shared, asked)")
        .expect("the census note");
    let dbg = miss.find("dbg_jitc()").expect("the diagnostic");
    assert!(note < dbg, "noted before (outside) the CRATONVM_DBG_JITC block");
}

/// Both of the bridge's leaf arms go through the lean door's gate, which
/// keeps the ladder for a `MethodHandle` leaf, a constructor and the
/// switch off, and runs the ladder's inline-cache quiescent point first.
#[test]
fn r22_runtime_both_bridge_leaf_arms_use_the_gated_lean_door() {
    let helpers = crate::jit::helpers::HELPERS_SRC;
    let bridge = helpers
        .split("unsafe fn jit_indy_bridge_body(")
        .nth(1)
        .expect("bridge body");
    let bridge = &bridge[..bridge.find("\n\u{7d}\n").expect("bridge end")];
    assert_eq!(bridge.matches("jit_indy_leaf_dispatch(vm_ptr, leaf,").count(), 2);
    let door = helpers
        .split("unsafe fn jit_indy_leaf_dispatch(")
        .nth(1)
        .expect("lean door");
    let door = &door[..door.find("\n\u{7d}\n").expect("door end")];
    for needle in [
        "leaf.class_name == \"java/lang/invoke/MethodHandle\"",
        "leaf.method_name == \"<init>\"",
        "indy_leaf_direct_call_enabled(",
        "cratonvm_jit::jit_ic_quiescent_point();",
        "mh_bind_direct_call(vm_ptr, leaf, args_ptr, arg_count)",
    ] {
        assert!(door.contains(needle), "{needle}");
    }
    let gate = door.find("indy_leaf_direct_call_enabled(").expect("gate");
    let quiescent = door.find("jit_ic_quiescent_point").expect("quiescent");
    let lean = door.find("mh_bind_direct_call(").expect("lean");
    assert!(gate < quiescent && quiescent < lean);
}
