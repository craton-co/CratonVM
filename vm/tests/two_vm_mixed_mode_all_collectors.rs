// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Proof test for `docs/architecture/per-vm-state.md`:
//! boots two VMs in one process in different modes (`--real-jdk` and
//! `--compatible`), runs the same class in both, drops one while the other
//! keeps running, and passes under all three collectors (Generational, G1, ZGC).

use cratonvm_vm::config::{resolve_java_home_public, GcAlgorithm, VmConfig};
use cratonvm_vm::native::jni::{java_vm_for, vm_for_java_vm};
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

fn test_resources_dir() -> String {
    format!("{}/tests/resources", env!("CARGO_MANIFEST_DIR"))
}

fn boot_pair(alg: GcAlgorithm) -> Option<(Vm, Vm)> {
    let java_home = resolve_java_home_public(None)?;

    let mut real_cfg = VmConfig::new()
        .with_classpath(vec![test_resources_dir()])
        .with_java_home(java_home.to_string_lossy().into_owned());
    real_cfg.use_synthetic_jdk = false;
    real_cfg.gc_algorithm = alg;
    let real_vm = Vm::new(real_cfg);

    let mut compat_cfg = VmConfig::new().with_classpath(vec![test_resources_dir()]);
    compat_cfg.use_synthetic_jdk = true;
    compat_cfg.gc_algorithm = alg;
    let compat_vm = Vm::new(compat_cfg);

    Some((real_vm, compat_vm))
}

fn assert_arithmetic(vm: &mut Vm, method: &str, expected: i32) {
    let val = vm
        .invoke("cratonvm/Arithmetic", method, "()I", &[])
        .unwrap_or_else(|e| panic!("invoke {method} failed: {e:?}"));
    assert_eq!(val, Some(Value::Int(expected)), "{method} result mismatch");
}

fn run_two_vm_lifecycle(alg: GcAlgorithm) {
    let Some((mut real_vm, mut compat_vm)) = boot_pair(alg) else {
        eprintln!("Skipping {alg:?}: no real JDK available on this host");
        return;
    };

    assert!(!real_vm.shared.config.use_synthetic_jdk);
    assert!(compat_vm.shared.config.use_synthetic_jdk);

    let h_real = java_vm_for(&real_vm.shared);
    let h_compat = java_vm_for(&compat_vm.shared);
    assert_ne!(h_real, h_compat);
    assert!(std::sync::Arc::ptr_eq(
        &vm_for_java_vm(h_real).expect("real handle resolves"),
        &real_vm.shared,
    ));
    assert!(std::sync::Arc::ptr_eq(
        &vm_for_java_vm(h_compat).expect("compat handle resolves"),
        &compat_vm.shared,
    ));

    // Run the same class in both VMs while both are live.
    assert_arithmetic(&mut real_vm, "test", 30);
    assert_arithmetic(&mut compat_vm, "test", 30);
    assert_arithmetic(&mut real_vm, "testMul", 42);
    assert_arithmetic(&mut compat_vm, "testMul", 42);

    // Drop the --real-jdk VM; --compatible VM keeps running.
    drop(real_vm);
    assert!(vm_for_java_vm(h_real).is_none());
    assert!(vm_for_java_vm(h_compat).is_some());
    assert_arithmetic(&mut compat_vm, "testDiv", 25);
    assert_arithmetic(&mut compat_vm, "testMod", 2);
    drop(compat_vm);

    // Reverse drop order: drop --compatible first, --real-jdk keeps running.
    let (mut real_vm, mut compat_vm) = boot_pair(alg).expect("second boot_pair");
    assert_arithmetic(&mut real_vm, "test", 30);
    assert_arithmetic(&mut compat_vm, "test", 30);
    drop(compat_vm);
    assert_arithmetic(&mut real_vm, "testDiv", 25);
    assert_arithmetic(&mut real_vm, "testNeg", -42);
    drop(real_vm);
}

#[test]
fn two_vm_mixed_mode_generational() {
    run_two_vm_lifecycle(GcAlgorithm::Generational);
}

#[test]
fn two_vm_mixed_mode_g1() {
    run_two_vm_lifecycle(GcAlgorithm::G1);
}

#[cfg(feature = "zgc")]
#[test]
fn two_vm_mixed_mode_zgc() {
    run_two_vm_lifecycle(GcAlgorithm::Zgc);
}
