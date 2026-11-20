use super::*;
use cratonvm_native_api::NativeKind;

fn jdk_only_vm() -> SharedVm {
    let mut config = crate::config::VmConfig::default();
    config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
    // `JdkOnly` with the synthetic library is a rejected pair.
    config.use_synthetic_jdk = false;
    SharedVm::new(config)
}

const T: (&str, &str, &str) = (
    "java/lang/ref/SoftReference",
    "refersTo",
    "(Ljava/lang/Object;)Z",
);

#[test]
fn jr21_refs2_compatible_other_kinds_and_unloaded_classes_keep_the_shadow() {
    let compat = SharedVm::new(crate::config::VmConfig::default());
    assert!(!bridge_yields_to_loaded_bytecode_for_gates(
        &compat,
        None,
        T.0,
        T.1,
        T.2,
        NativeKind::Bridge
    ));
    let strict = jdk_only_vm();
    for kind in [NativeKind::Intrinsic, NativeKind::SyntheticStub] {
        assert!(!bridge_yields_to_loaded_bytecode_for_gates(
            &strict, None, T.0, T.1, T.2, kind
        ));
    }
    // A fresh VM has not loaded the class: no `Code` to yield to.
    assert!(!bridge_yields_to_loaded_bytecode_for_gates(
        &strict,
        None,
        "com/example/NeverLoaded",
        T.1,
        T.2,
        NativeKind::Bridge
    ));
}

/// Text pins: `registered_native_will_run`, `inline_screen_native_shadows`
/// and the callee door's inherited-native check ask the rule.
#[test]
fn jr21_refs2_the_three_gates_ask_the_rule() {
    const END: &str = "\n\x7d\n";
    let here = include_str!("../native_override.rs").replace("\r\n", "\n");
    let will_run = &here[here
        .find("pub(crate) fn registered_native_will_run(")
        .expect("will_run")..];
    let will_run = &will_run[..will_run.find(END).expect("end")];
    assert!(will_run.contains("bridge_yields_to_loaded_bytecode_for_gates("));
    let bridge = concat!(include_str!("../jit_bridge/mod.rs"), "\n", include_str!("../jit_bridge/jit_verdicts.rs"), "\n", include_str!("../jit_bridge/osr_continuation.rs"), "\n", include_str!("../jit_bridge/compile_osr_body.rs"), "\n", include_str!("../jit_bridge/osr_exception_exit.rs"), "\n", include_str!("../jit_bridge/try_osr.rs"), "\n", include_str!("../jit_bridge/native_shadow_arms.rs"), "\n", include_str!("../jit_bridge/compile_optimizing_artifact.rs"), "\n", include_str!("../jit_bridge/never_run_static_exit_pcs.rs"), "\n", include_str!("../jit_bridge/try_jit_compile_callee_slow.rs"), "\n", include_str!("../jit_bridge/byte_array_field_declarer_admitted.rs"), "\n", include_str!("../jit_bridge/folded_handle_member_row.rs"), "\n", include_str!("../jit_bridge/callee_never_taken_rows.rs"), "\n", include_str!("../jit_bridge/mint_ldc_slots_at_offer.rs")).replace("\r\n", "\n");
    let screen = &bridge[bridge
        .find("fn inline_screen_native_shadows(")
        .expect("screen")..];
    let screen = &screen[..screen.find(END).expect("end")];
    assert!(screen.contains("bridge_yields_to_loaded_bytecode_for_gates("));
    let inherited = bridge
        .find("\"vm-native-override-present\"")
        .expect("inherited check");
    let gate = bridge[..inherited]
        .rfind("bridge_yields_to_loaded_bytecode_for_gates(")
        .expect("asked before the refusal");
    assert!(inherited - gate < 1500, "the inherited check asks the rule");
    assert!(here.contains(concat!("dispatch_policy(shared).is_jdk_", "only()")));
}

/// JIT round 21 lane refs221 (REFS221-3): the rule asks the by-name
/// doors' own walk (`vm_exec`) and walks nothing itself, so the resolve
/// guard's `native_override.rs` row is back to 3.
#[test]
fn jr21_refs221_the_gate_rule_asks_the_by_name_doors_walk() {
    const END: &str = "\n\x7d\n";
    let here = include_str!("../native_override.rs").replace("\r\n", "\n");
    let rule = &here[here
        .find("pub(crate) fn bridge_yields_to_loaded_bytecode_for_gates(")
        .expect("rule")..];
    let rule = &rule[..rule.find(END).expect("end")];
    assert!(rule.contains("registered_bridge_yields_to_loaded_bytecode_census_free("));
    // Spelled in two pieces: the resolve guard counts the literal.
    assert!(!rule.contains(concat!("find_method", "_recursive(")));
    let exec = concat!(include_str!("../../../vm/vm_exec/mod.rs"), "\n", include_str!("../../../vm/vm_exec/method.rs"), "\n", include_str!("../../../vm/vm_exec/report_pad_access.rs"), "\n", include_str!("../../../vm/vm_exec/diagnose_sync_on_value_based_class.rs"), "\n", include_str!("../../../vm/vm_exec/thread_interrupted_field_slot_of.rs"), "\n", include_str!("../../../vm/vm_exec/clear_breakpoints_of_redefined_class.rs"), "\n", include_str!("../../../vm/vm_exec/load_class.rs"), "\n", include_str!("../../../vm/vm_exec/native_new_initializes_class_first.rs"), "\n", include_str!("../../../vm/vm_exec/new_object.rs"), "\n", include_str!("../../../vm/vm_exec/gen_referent_read_keep_alive.rs"), "\n", include_str!("../../../vm/vm_exec/append_native_standin_frames.rs"), "\n", include_str!("../../../vm/vm_exec/unloaded_native_libraries.rs"), "\n", include_str!("../../../vm/vm_exec/extract_annotations_from_attributes.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_static_settling_with_first.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_special_shared.rs"), "\n", include_str!("../../../vm/vm_exec/proxy_resolve_declaring_method.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_on_class_shared_inner.rs"), "\n", include_str!("../../../vm/vm_exec/jni_table_native_call.rs")).replace("\r\n", "\n");
    let free = &exec[exec
        .find("pub(crate) fn registered_bridge_yields_to_loaded_bytecode_census_free(")
        .expect("census-free door")..];
    let free = &free[..free.find(END).expect("end")];
    // No registration is passed: `resolve_dispatch` records the shadow
    // census only for a passed registration.
    assert!(free.contains("selected_body_dispatches_bytecode("));
    assert!(!free.contains("Some((callback, kind))"));
}
