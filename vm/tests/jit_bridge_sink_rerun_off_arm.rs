// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The OFF arm of `CRATONVM_JIT_DEOPT_SINK_RESUME` for the `jit_bridge` sinks,
//! in its own process.
//!
//! `jit_bridge_sink_resumes_instead_of_rerunning.rs` asserts the side effect
//! runs ONCE. On its own that is one outcome from one binary, and an outcome
//! alone does not say the switch is what produced it — the method might simply
//! have stopped trapping, or stopped reaching this sink. This file runs the
//! same fixture with the resume switched off and asserts the original
//! duplication, so the pair is a real A/B: same binary, same fixture, one
//! switch, two outcomes.
//!
//! It is also what keeps the switch honest. A default-ON knob whose OFF arm
//! nothing exercises is a knob that quietly stops working, and the next person
//! to reach for it during a bisect finds out the hard way.
//!
//! # Why a separate test binary
//!
//! `deopt_sink_resume_enabled` is a `OnceLock`, latched on first read. Two arms
//! in one process would both see whichever value the first latched, and the
//! second would pass or fail for a reason unrelated to its own override.
//!
//! # This asserts a DEFECT, deliberately
//!
//! A delta of 2 is a silent duplicated side effect — the thing the default arm
//! exists to prevent. Asserting it here is not endorsing it: it is pinning that
//! the switch still reaches the sink, which is the only thing that makes the ON
//! arm's result evidence rather than a coincidence.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

const CLASS: &str = "cratonvm/DeoptRerunCount";
const HOT: &str = "hot";
const HOT_DESC: &str = "(II)I";
const WARM_CALLS: i32 = 200;
const ITERS_PER_WARM: i32 = 64;

/// The sibling file's overrides, plus the kill switch.
///
/// Every row is load-bearing; see that file for why each one is there. Without
/// the tier rows the method sits at C1, where `can_deopt_resume` is set and the
/// sink's pre-existing arm resumes whatever this switch says.
const OVERRIDES: &[(&str, Option<&str>)] = &[
    ("CRATONVM_TIER_C1_THRESHOLD", Some("100000")),
    ("CRATONVM_TIER_C2_THRESHOLD", Some("600")),
    ("CRATONVM_TIER_C2_MIN_INVOCATIONS", Some("500")),
    ("CRATONVM_C2_ACCEPT", Some("always")),
    ("CRATONVM_JIT_DEOPT_SINK_RESUME", Some("0")),
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
            .get(CLASS, HOT, HOT_DESC, class_id)
        {
            if compiled.used_ir_backend {
                return;
            }
        }
        for _ in 0..CALLS_PER_ATTEMPT {
            let _ = vm.invoke(CLASS, "warm", "(I)I", &[Value::Int(ITERS_PER_WARM)]);
        }
    }
    panic!(
        "{CLASS}.{HOT} never reached the OPTIMIZING backend — this arm is about what the switch \
         does to an IR body, so proceeding would measure the single-pass backend and pass, or \
         fail, for the wrong reason"
    );
}

#[test]
fn with_the_resume_switched_off_the_side_effect_runs_twice() {
    cratonvm_types::flags::with_process_overrides(OVERRIDES, body);
}

fn body() {
    let mut vm = Vm::new(VmConfig::new().with_classpath(vec![test_resources_dir()]));

    for _ in 0..WARM_CALLS {
        let _ = vm.invoke(CLASS, "warm", "(I)I", &[Value::Int(ITERS_PER_WARM)]);
    }
    wait_until_optimizing_compiled(&mut vm);
    let _ = vm.invoke(CLASS, "reset", "()V", &[]);

    let delta = match vm.invoke(CLASS, "trip", "()I", &[]) {
        Ok(Some(Value::Int(n))) => n,
        other => panic!("trip() did not return an int: {other:?}"),
    };

    assert_eq!(
        delta, 2,
        "with the resume switched off this sink must still re-run the body from entry, which \
         runs the store twice. A delta of {delta} means the switch no longer reaches \
         `execute_jit_call`'s sink, and the ON arm's result stopped being evidence that the \
         switch is what produces it"
    );
}
