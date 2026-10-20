// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The OFF arm of `CRATONVM_JIT_DEOPT_SINK_RESUME` for the `jit_bridge` sinks,
//! in its own process.
//!
//! `jit_bridge_sink_resumes_instead_of_rerunning.rs` asserts the side effect
//! runs ONCE out of an OPTIMIZING body. On its own that is one outcome from one
//! binary, and an outcome alone does not say the switch is what produced it.
//! This file runs the same fixture with the resume switched off, so the pair is
//! a real A/B: same binary, same fixture, one switch, two outcomes.
//!
//! It is also what keeps the switch honest. A default-ON knob whose OFF arm
//! nothing exercises is a knob that quietly stops working, and the next person
//! to reach for it during a bisect finds out the hard way.
//!
//! # The witness changed on 2026-10-01 (gcf f1/r)
//!
//! Until the JIT round-12 replay gate (`f5f63cb78`, 2026-09-26) this file
//! asserted the DEFECT the switch guards against: with the resume off, the
//! sink re-ran `hot` from entry and the `iastore` ran twice (delta 2). That
//! outcome no longer exists, by design. The replay gate in `cratonvm_jit`
//! (`first_point_needing_unsound_replay_of_method`, the body-wide half of which
//! reads `deopt_sink_resume_enabled()`) now refuses, at COMPILE time, an
//! optimizing body whose deopt could only be replayed from entry and would
//! commit a side effect again:
//!
//! ```text
//! [ir] cratonvm/DeoptRerunCount.hot(II)I: the deopt at bci 14 can only be
//!      replayed from entry and the replay would commit a side effect again
//!      -- keeping the single-pass body
//! ```
//!
//! So under `CRATONVM_JIT_DEOPT_SINK_RESUME=0` the old witness's warm-up never
//! saw an IR body and panicked. The switch still decides something observable,
//! and that is the new witness:
//!
//! | arm | optimizing body published | delta |
//! |---|---|---|
//! | ON (sibling file) | yes | 1 (precise resume) |
//! | OFF (this file)   | **never** (refused by the replay gate) | 1 (no duplicate) |
//!
//! Both halves are asserted. "Never published" with the ON arm's budget is what
//! proves the switch still reaches the JIT; delta 1 is what proves the refusal
//! protects the side effect rather than merely moving the duplication
//! somewhere else.
//!
//! # Why a separate test binary
//!
//! `deopt_sink_resume_enabled` is a `OnceLock`, latched on first read. Two arms
//! in one process would both see whichever value the first latched, and the
//! second would pass or fail for a reason unrelated to its own override.

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
/// Every row is load-bearing; see that file for why each one is there.
/// `CRATONVM_C2_ACCEPT=always` matters doubly here: it removes the acceptance
/// gate as an alternative reason for "no IR body", so the only gate left to
/// refuse it is the replay gate this switch feeds.
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

/// Drive `hot` with the same budget the ON arm uses to reach the optimizing
/// backend, and return whether an IR body was ever published.
fn optimizing_body_published_within_budget(vm: &mut Vm) -> bool {
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
                return true;
            }
        }
        for _ in 0..CALLS_PER_ATTEMPT {
            let _ = vm.invoke(CLASS, "warm", "(I)I", &[Value::Int(ITERS_PER_WARM)]);
        }
    }
    false
}

#[test]
fn with_the_resume_switched_off_the_replay_gate_keeps_the_single_pass_body() {
    cratonvm_types::flags::with_process_overrides(OVERRIDES, body);
}

fn body() {
    let mut vm = Vm::new(VmConfig::new().with_classpath(vec![test_resources_dir()]));

    for _ in 0..WARM_CALLS {
        let _ = vm.invoke(CLASS, "warm", "(I)I", &[Value::Int(ITERS_PER_WARM)]);
    }
    assert!(
        !optimizing_body_published_within_budget(&mut vm),
        "{CLASS}.{HOT} reached the OPTIMIZING backend with CRATONVM_JIT_DEOPT_SINK_RESUME=0. \
         Its deopt at the idiv can then only be replayed from entry, and the replay re-runs \
         the iastore: the round-12 replay gate must refuse that body. Either the switch no \
         longer reaches `first_point_needing_unsound_replay_of_method`, or the gate stopped \
         refusing a side-effecting replay"
    );
    let _ = vm.invoke(CLASS, "reset", "()V", &[]);

    let delta = match vm.invoke(CLASS, "trip", "()I", &[]) {
        Ok(Some(Value::Int(n))) => n,
        other => panic!("trip() did not return an int: {other:?}"),
    };

    assert_eq!(
        delta, 1,
        "with the optimizing body refused, the body that traps is the single-pass one (or the \
         interpreter), and its side effect must run exactly once. A delta of {delta} means the \
         refusal did not protect the store"
    );
}
