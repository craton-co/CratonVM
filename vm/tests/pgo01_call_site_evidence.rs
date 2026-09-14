// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! PGO-01 (pgo-01-call-site-evidence-gap.md, retired to
//! docs/internal): per-call-site evidence for `invokestatic`/`invokespecial`.
//!
//! `MethodProfile::call_sites` (jit/src/profile.rs) is fed exclusively from
//! the interpreter's invoke dispatch in `vm/src/runtime/interpreter/invoke.rs`
//! (`execute_invokestatic`, `execute_invokestatic_cached`, `execute_invoke_kind`
//! and `execute_invokevirtual_cached` gated on `is_special`). This is an
//! end-to-end check through a real interpreted run — `jit/src/profile.rs`'s
//! own unit tests call `record_call_site` directly and never exercise the
//! interpreter at all, so they cannot catch a wiring gap like the one this
//! lane closed (the recorder existed with zero callers).

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::jit::profile::{
    enable_profiling, enable_receiver_profiling, is_receiver_profiling_enabled, MethodKey,
};
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;
use std::sync::Arc;

fn test_resources_dir() -> String {
    if let Some(generated) = option_env!("CRATONVM_TEST_CLASSES_DIR") {
        if std::path::Path::new(generated).is_dir() {
            return generated.to_owned();
        }
    }
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn class_files_available() -> bool {
    let dir = test_resources_dir();
    let class_path = format!("{dir}/cratonvm/PgoCallSiteEvidence.class");
    std::path::Path::new(&class_path).exists()
}

fn test_vm() -> Vm {
    let config = VmConfig::new().with_classpath(vec![test_resources_dir()]);
    Vm::new(config)
}

/// Well under `CRATONVM_JIT_THRESHOLD`'s default (500), so every call in this
/// test stays interpreted — no JIT-tiering nondeterminism to account for, and
/// exact counts are meaningful (no "no wall-clock bound" flake risk either,
/// per the doc's "How to verify").
const LOOP_COUNT: i32 = 20;

fn class_id_of(vm: &Vm, class_name: &str) -> u32 {
    vm.shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id(class_name)
        .unwrap_or_else(|| panic!("{class_name} must be loaded after invoke"))
        .as_u32()
}

#[test]
fn test_pgo01_call_site_evidence() {
    if !class_files_available() {
        eprintln!(
            "Skipping test_pgo01_call_site_evidence: .class files not available (javac not on PATH?)"
        );
        return;
    }

    // --- Positive control 1: invokestatic call site -------------------------
    enable_profiling(true);
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/PgoCallSiteEvidence",
        "callStaticLoop",
        "(I)I",
        &[Value::Int(LOOP_COUNT)],
    );
    assert!(
        matches!(result, Ok(Some(Value::Int(_)))),
        "callStaticLoop failed: {result:?}"
    );

    let class_id = class_id_of(&vm, "cratonvm/PgoCallSiteEvidence");
    let static_key = MethodKey {
        class_id,
        method_name: Arc::from("callStaticLoop"),
        descriptor: Arc::from("(I)I"),
    };
    let static_profile = vm
        .shared
        .jit
        .profile_store
        .get_profile(&static_key)
        .expect("callStaticLoop must have a profile once profiling is enabled");
    assert!(
        !static_profile.call_sites.is_empty(),
        "invokestatic call site was not recorded: {:?}",
        static_profile.call_sites
    );
    let static_total: u32 = static_profile.call_sites.values().sum();
    assert_eq!(
        static_total, LOOP_COUNT as u32,
        "expected exactly {LOOP_COUNT} invokestatic executions of staticHelper, got {static_total} (call_sites={:?})",
        static_profile.call_sites
    );

    // --- Positive control 2: invokespecial call site -------------------------
    let result = vm.invoke(
        "cratonvm/PgoCallSiteEvidence",
        "callSpecialLoop",
        "(I)I",
        &[Value::Int(LOOP_COUNT)],
    );
    assert!(
        matches!(result, Ok(Some(Value::Int(_)))),
        "callSpecialLoop failed: {result:?}"
    );

    let special_key = MethodKey {
        class_id,
        method_name: Arc::from("callSpecialLoop"),
        descriptor: Arc::from("(I)I"),
    };
    let special_profile = vm
        .shared
        .jit
        .profile_store
        .get_profile(&special_key)
        .expect("callSpecialLoop must have a profile once profiling is enabled");
    assert!(
        !special_profile.call_sites.is_empty(),
        "invokespecial call site was not recorded: {:?}",
        special_profile.call_sites
    );
    // The loop's INSTANCE.specialHelper(i) call site executes LOOP_COUNT
    // times; take the max rather than the sum in case some other bci in this
    // method's bytecode (there shouldn't be one) also recorded.
    let special_max: u32 = special_profile
        .call_sites
        .values()
        .copied()
        .max()
        .unwrap_or(0);
    assert_eq!(
        special_max, LOOP_COUNT as u32,
        "expected the invokespecial call site to have executed {LOOP_COUNT} times, got {special_max} (call_sites={:?})",
        special_profile.call_sites
    );

    // --- Negative control 1: an invokevirtual-only method must not feed
    // call_sites at all — that evidence stays in `receivers` only. -----------
    let result = vm.invoke(
        "cratonvm/PgoCallSiteEvidence",
        "callVirtualOnlyLoop",
        "(I)I",
        &[Value::Int(LOOP_COUNT)],
    );
    assert!(
        matches!(result, Ok(Some(Value::Int(_)))),
        "callVirtualOnlyLoop failed: {result:?}"
    );
    let virtual_key = MethodKey {
        class_id,
        method_name: Arc::from("callVirtualOnlyLoop"),
        descriptor: Arc::from("(I)I"),
    };
    let virtual_profile = vm.shared.jit.profile_store.get_profile(&virtual_key);
    let virtual_call_sites_empty = virtual_profile
        .as_ref()
        .map(|p| p.call_sites.is_empty())
        .unwrap_or(true);
    assert!(
        virtual_call_sites_empty,
        "a method with only invokevirtual call sites must not populate call_sites: {:?}",
        virtual_profile.map(|p| p.call_sites)
    );

    // --- Negative control 2: same invokestatic shape, profiling disabled —
    // the gate itself must actually gate. A fresh Vm avoids any interference
    // from the profiling-enabled run above. ----------------------------------
    //
    // THERE ARE TWO GATES, AND CONSTRUCTING A VM RE-ARMS ONE OF THEM.
    // `is_receiver_profiling_enabled()` is `PROFILING_ENABLED ||
    // RECEIVER_PROFILING_ENABLED`; the second landed on 2026-09-02 so that
    // receiver and call-site recording could be on by default while branch and
    // back-edge recording stayed behind the master gate. `enable_profiling`
    // clears only the master one, and `Vm::new` then calls
    // `enable_receiver_profiling(tier_pgo_receivers())`, which DEFAULTS TRUE —
    // so this control disabled nothing and the assertion below found
    // `{11: 2}`. It read as "the gate leaks"; the gate was fine and the control
    // was defeated by the VM it was about to measure.
    //
    // So: clear BOTH, and clear them AFTER construction. The assertion that the
    // gate is really shut is the load-bearing part — without it a third gate
    // would silently defeat this control the same way.
    enable_profiling(false);
    let mut vm2 = test_vm();
    enable_receiver_profiling(false);
    assert!(
        !is_receiver_profiling_enabled(),
        "the negative control must actually disable recording before it can claim \n         the gate works: `Vm::new` re-arms the receiver gate"
    );
    let result = vm2.invoke(
        "cratonvm/PgoCallSiteEvidence",
        "callStaticLoop",
        "(I)I",
        &[Value::Int(LOOP_COUNT)],
    );
    assert!(
        matches!(result, Ok(Some(Value::Int(_)))),
        "callStaticLoop (profiling disabled) failed: {result:?}"
    );
    let class_id2 = class_id_of(&vm2, "cratonvm/PgoCallSiteEvidence");
    let static_key2 = MethodKey {
        class_id: class_id2,
        method_name: Arc::from("callStaticLoop"),
        descriptor: Arc::from("(I)I"),
    };
    let disabled_profile = vm2.shared.jit.profile_store.get_profile(&static_key2);
    let disabled_call_sites_empty = disabled_profile
        .as_ref()
        .map(|p| p.call_sites.is_empty())
        .unwrap_or(true);
    assert!(
        disabled_call_sites_empty,
        "call_sites must stay empty while profiling is disabled: {:?}",
        disabled_profile.map(|p| p.call_sites)
    );
}
