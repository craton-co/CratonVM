// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP4.6 — ConcurrentHashMap regression tests.
//!
//! Pins the K1-family coercion fix at `vm/src/runtime/value_stack.rs:385-460`
//! (Value::Object(None) and Value::Uninitialized → 0 in pop_int/pop_long).
//! Without that fix, ConcurrentHashMap.initTable enters a CAS livelock when
//! reading the default-zero `sizeCtl` primitive int slot — the CAS retry loop
//! reads `Value::Object(None)` instead of `Value::Int(0)` and
//! `Unsafe.compareAndSetInt(expected=0, ...)` fails forever.
//!
//! `test_chm_basic_put_get` is the WP4.6 acceptance criterion: 1000 puts past
//! the default 16-bucket initial capacity → forces ≥1 transfer() resize pass.
//!
//! `test_chm_pre_resize_put_get` is the smallest no-resize baseline:
//! 11 puts stays under the 0.75 × 16 = 12 entry resize threshold so transfer()
//! is never invoked. It also pins the default-mode autoboxing surface used by
//! CHM's boxed `Integer` values.
//!
//! Sibling probes verify resize, mutation cycles, and clear/isEmpty invariants
//! on the same JDK 25 ConcurrentHashMap.
//!
//! # Every String-keyed test here was `#[ignore]`d until 2026-08-04
//!
//! Under two separate labels — "CHM `get()` misses a key the same VM just
//! stored, in-process only" and "WP4.6-FOLLOWUP-A: CHM transfer() data-loss
//! after resize past 16 buckets" — and they were one bug, in neither CHM nor
//! `transfer()`. `NativeContextImpl::java_strings_equal` answered `false` (not
//! "I did not read these") for two Strings whose character storage it could
//! not decode, and this VM's fabricated `java/lang/String` is `char[]`-backed
//! rather than the JDK-9+ `byte[]`+`coder` layout it assumed. `CHM.get`
//! believed that `false`. `HashMap.get`'s String fast path compares decoded
//! text and never asked, which is why HashMap looked healthy throughout, and
//! the Integer-keyed `test_chm_resize_path` was ignored for a resize defect it
//! never had.
//!
//! So a String-keyed failure here is evidence about String equality first and
//! about CHM second: read `vm/src/vm/vm_exec.rs`'s `java_string_storage`
//! before any CHM code.
//!
//! See `apps/chm_basic/ChmBasic.java` for the standalone CLI variant of the
//! same probe (used as a smoke test for the cratonvm.exe binary).
//! See `apps/chm_stress/ChmStress.java` for the contention stress probe.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::error::{MethodCallFailed, MethodCallResult};
use cratonvm_vm::memory::ArrayElementType;
use cratonvm_vm::types::ObjectRef;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

/// Path to the test resources directory (matches interpreter_tests.rs).
fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn test_classpath() -> Vec<String> {
    let mut cp = Vec::new();
    if let Some(compiled) = option_env!("CRATONVM_TEST_CLASSES_DIR") {
        cp.push(compiled.to_string());
    }
    cp.push(test_resources_dir());
    cp
}

/// Check whether build.rs compiled the JDK21+ fixtures or committed fallback
/// classes are present.
fn class_files_available() -> bool {
    test_classpath().into_iter().any(|dir| {
        let class_path = format!("{dir}/cratonvm/ChmBasicProbe.class");
        std::path::Path::new(&class_path).exists()
    })
}

fn test_vm() -> Vm {
    let config = VmConfig::new().with_classpath(test_classpath());
    Vm::new(config)
}

fn read_test_java_string(vm: &Vm, obj: ObjectRef) -> Option<String> {
    let heap = &vm.shared.mem.heap;
    let value_array = match heap.get_field(obj, 0) {
        Value::Object(Some(arr)) => arr,
        _ => return None,
    };

    match heap.array_element_type(value_array) {
        Some(ArrayElementType::Byte) => {
            let coder = match heap.get_field(obj, 1) {
                Value::Int(value) => value,
                _ => 0,
            };
            let bytes: Vec<u8> = (0..heap.array_length(value_array))
                .filter_map(|i| match heap.get_array_element(value_array, i).ok()? {
                    Value::Int(value) => Some(value as u8),
                    _ => None,
                })
                .collect();
            if coder == 1 {
                let units: Vec<u16> = bytes
                    .chunks_exact(2)
                    .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                    .collect();
                Some(String::from_utf16_lossy(&units))
            } else {
                Some(bytes.into_iter().map(char::from).collect())
            }
        }
        Some(ArrayElementType::Char) => {
            let units: Vec<u16> = (0..heap.array_length(value_array))
                .filter_map(|i| match heap.get_array_element(value_array, i).ok()? {
                    Value::Int(value) => Some(value as u16),
                    _ => None,
                })
                .collect();
            Some(String::from_utf16_lossy(&units))
        }
        _ => None,
    }
}

fn throwable_detail_message(vm: &Vm, exc: ObjectRef) -> Option<String> {
    let class_id = vm.shared.mem.heap.class_id_of(exc);
    let msg_ref = vm
        .instance_field_index(class_id, "detailMessage")
        .and_then(|idx| match vm.shared.mem.heap.get_field(exc, idx) {
            Value::Object(Some(msg)) => Some(msg),
            _ => None,
        })?;
    read_test_java_string(vm, msg_ref)
}

fn describe_result(vm: &Vm, result: &MethodCallResult) -> String {
    match result {
        Err(MethodCallFailed::ExceptionThrown(exc)) => {
            let class_id = vm.shared.mem.heap.class_id_of(*exc);
            let class_name = vm
                .shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .map(|class| class.name.to_string())
                .unwrap_or_else(|| format!("<unknown class {:?}>", class_id));
            match throwable_detail_message(vm, *exc) {
                Some(message) => {
                    format!("Err(ExceptionThrown({class_name}: {message}, {exc:?}))")
                }
                None => format!("Err(ExceptionThrown({class_name}, {exc:?}))"),
            }
        }
        other => format!("{other:?}"),
    }
}

macro_rules! require_class_files {
    () => {
        if !class_files_available() {
            eprintln!("Skipping: ChmBasicProbe.class not available (javac not on PATH or build.rs failed)");
            return;
        }
    };
}

/// Baseline without resize: 11 entries -> no `transfer()` path.
/// Pins boxed `Integer` put/get through default-mode autoboxing.
#[test]
fn test_chm_pre_resize_put_get() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke(
        "cratonvm/ChmBasicProbe",
        "testChmPreResizePutGet",
        "()I",
        &[],
    );
    match result {
        Ok(Some(Value::Int(1))) => {}
        other => {
            panic!(
                "ChmBasicProbe.testChmPreResizePutGet expected Ok(Some(Int(1))), got: {}",
                describe_result(&vm, &other)
            )
        }
    }
}

/// WP4.6 acceptance gate: 1000 puts then gets.
///
/// Note what this catches and what it does not. It returns 0 for *any*
/// failure, so it cannot say which key was lost — that is why the diagnosis
/// ran through `test_chm_pre_resize_put_get`'s staged return codes instead.
/// Its value is scale: 1000 String keys across 16 segments do force real
/// per-segment growth, so it is the pin that a resize genuinely relinks
/// chains.
#[test]
fn test_chm_basic_put_get() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ChmBasicProbe", "testChmBasicPutGet", "()I", &[]);
    match result {
        Ok(Some(Value::Int(1))) => {}
        other => {
            panic!(
                "ChmBasicProbe.testChmBasicPutGet expected Ok(Some(Int(1))), got: {}",
                describe_result(&vm, &other)
            )
        }
    }
}

/// 64-entry resize probe, keyed by `Integer` rather than `String`. It is the
/// control for its String-keyed siblings: it passed throughout the 2026-08-03
/// investigation (it was `#[ignore]`d on a resize hypothesis it never
/// confirmed), which is what located the defect in String equality rather
/// than in the resize path both keys share.
#[test]
fn test_chm_resize_path() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ChmBasicProbe", "testChmResizePath", "()I", &[]);
    match result {
        Ok(Some(Value::Int(1))) => {}
        other => {
            panic!(
                "ChmBasicProbe.testChmResizePath expected Ok(Some(Int(1))), got: {}",
                describe_result(&vm, &other)
            )
        }
    }
}

/// Single-key mutation cycle -> fits in one bucket, no resize.
/// Pins boxed values through putIfAbsent/replace/remove.
#[test]
fn test_chm_mutation_cycle() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ChmBasicProbe", "testChmMutationCycle", "()I", &[]);
    match result {
        Ok(Some(Value::Int(1))) => {}
        other => {
            panic!(
                "ChmBasicProbe.testChmMutationCycle expected Ok(Some(Int(1))), got: {}",
                describe_result(&vm, &other)
            )
        }
    }
}

/// Regression pin for VM-scoped wrapper caches: boxed Integer cache entries
/// from one VM instance must not be reused by the next VM in the same Rust test
/// process.
#[test]
fn test_chm_boxed_cache_is_vm_scoped() {
    require_class_files!();

    let mut first_vm = test_vm();
    let first = first_vm.invoke("cratonvm/ChmBasicProbe", "testChmMutationCycle", "()I", &[]);
    match first {
        Ok(Some(Value::Int(1))) => {}
        other => {
            panic!(
                "first VM ChmBasicProbe.testChmMutationCycle expected Ok(Some(Int(1))), got: {}",
                describe_result(&first_vm, &other)
            )
        }
    }

    let mut second_vm = test_vm();
    let second = second_vm.invoke(
        "cratonvm/ChmBasicProbe",
        "testChmPreResizePutGet",
        "()I",
        &[],
    );
    match second {
        Ok(Some(Value::Int(1))) => {}
        other => {
            panic!(
                "second VM ChmBasicProbe.testChmPreResizePutGet expected Ok(Some(Int(1))), got: {}",
                describe_result(&second_vm, &other)
            )
        }
    }
}

/// Clear + isEmpty + size invariants on a 50-entry map. The clear path drops
/// the table reference rather than walking it, so this works even when
/// transfer() is broken — this test stays in the always-on suite as a sanity
/// pin for clear/isEmpty.
#[test]
fn test_chm_clear_empty() {
    require_class_files!();
    let mut vm = test_vm();
    let result = vm.invoke("cratonvm/ChmBasicProbe", "testChmClearEmpty", "()I", &[]);
    match result {
        Ok(Some(Value::Int(1))) => {}
        other => {
            panic!(
                "ChmBasicProbe.testChmClearEmpty expected Ok(Some(Int(1))), got: {}",
                describe_result(&vm, &other)
            )
        }
    }
}
