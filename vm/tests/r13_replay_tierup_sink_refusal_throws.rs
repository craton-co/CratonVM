// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `execute`'s first-call tier-up sink answers an unresumable own stash whose
//! replay would commit again with a Java `InternalError`, as the `jit_bridge`
//! doors do (round 13 wave 1, lane replay;
//! `r12w4-replay2-replay-sinks-residual-internalerror-and-reruns`).
//!
//! The fixture and the arm are `jit_deopt_sink_resume_off_arm.rs`'s: the
//! resume is switched off (`CRATONVM_JIT_DEOPT_SINK_RESUME=0`) and the
//! compile-time replay check with it, so an optimizing body of
//! `CompiledNpeMessage.storeToNull` is installed whose null-check trap no arm
//! can resume and whose replay would store again. Before this wave the sink
//! returned the VM-internal `VmError::Internal`, which no Java `catch` sees
//! and which ends the calling thread; the answer is now a throwable the
//! caller receives at the call. `CRATONVM_JIT_TIERUP_SINK_REFUSAL_THROWS=0`
//! restores the old answer (the off-arm file pins that one).
//!
//! One process per arm: the switches are latched on first read.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::error::MethodCallFailed;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

const CLASS: &str = "cratonvm/CompiledNpeMessage";
const METHOD: &str = "storeToNull";
const DESC: &str = "(I)I";
const WARM_CALLS: i32 = 3_000;

const OVERRIDES: &[(&str, Option<&str>)] = &[
    ("CRATONVM_TIER_C1_THRESHOLD", Some("100000")),
    ("CRATONVM_TIER_C2_THRESHOLD", Some("600")),
    ("CRATONVM_TIER_C2_MIN_INVOCATIONS", Some("500")),
    ("CRATONVM_JIT_DEOPT_SINK_RESUME", Some("0")),
    ("CRATONVM_JIT_IR_REPLAY_FALLBACK_CHECK", Some("0")),
];

fn test_resources_dir() -> String {
    if let Some(generated) = option_env!("CRATONVM_TEST_CLASSES_DIR") {
        if std::path::Path::new(generated).is_dir() {
            return generated.to_owned();
        }
    }
    format!("{}/tests/resources", env!("CARGO_MANIFEST_DIR"))
}

fn wait_until_optimizing_compiled(vm: &mut Vm) {
    const ATTEMPTS: usize = 400;
    const CALLS_PER_ATTEMPT: i32 = 50;
    for _ in 0..ATTEMPTS {
        let class_id = vm
            .shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(CLASS)
            .expect("the fixture class must be loaded after invoke");
        if let Some(compiled) = vm
            .shared
            .jit
            .jit_cache
            .read()
            .get(CLASS, METHOD, DESC, class_id)
        {
            if compiled.used_ir_backend {
                return;
            }
        }
        for _ in 0..CALLS_PER_ATTEMPT {
            let _ = vm.invoke(CLASS, METHOD, DESC, &[Value::Int(1)]);
        }
    }
    panic!(
        "{CLASS}.{METHOD} never reached the OPTIMIZING backend; the refusal this file pins is \
         the sink's answer to an IR body's trap"
    );
}

#[test]
fn the_tierup_sink_refusal_is_a_java_internal_error() {
    cratonvm_types::flags::with_process_overrides(OVERRIDES, body);
}

fn body() {
    let mut vm = Vm::new(VmConfig::new().with_classpath(vec![test_resources_dir()]));

    for _ in 0..WARM_CALLS {
        let _ = vm.invoke(CLASS, METHOD, DESC, &[Value::Int(1)]);
    }
    wait_until_optimizing_compiled(&mut vm);

    let err = match vm.invoke(CLASS, METHOD, DESC, &[Value::Int(0)]) {
        Err(e) => e,
        Ok(v) => panic!("{METHOD}(0) returned {v:?} instead of raising"),
    };
    let MethodCallFailed::ExceptionThrown(exc) = err else {
        panic!(
            "the tier-up sink's refusal must be a Java throwable the caller can catch, not a \
             VM-internal error; got {err:?}"
        );
    };
    let class_id = vm.shared.mem.heap.class_id_of(exc);
    let name = vm
        .shared
        .classes
        .class_manager
        .read()
        .get_class(class_id)
        .map(|c| c.name.to_string())
        .unwrap_or_else(|| format!("<unknown class {class_id:?}>"));
    assert_eq!(
        name, "java/lang/InternalError",
        "the refusal is the doors' `InternalError` (`refusing side-effecting replay`)"
    );

    // The calling thread survives, and the method was retired from
    // compilation with the refusal: an ordinary call still answers.
    match vm.invoke(CLASS, METHOD, DESC, &[Value::Int(1)]) {
        Ok(_) => {}
        Err(e) => panic!("{METHOD}(1) after the refusal failed: {e:?}"),
    }
}
