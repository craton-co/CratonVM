// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The OFF arm of `CRATONVM_JIT_DEOPT_SINK_RESUME`, in its own process.
//!
//! `jit_deopt_sink_resumes_a_side_effecting_trap.rs` asserts that a trap taken
//! in an optimizing-tier body resumes. On its own that is one outcome from one
//! binary, and an outcome alone does not say the switch is what produced it —
//! the method might simply have stopped trapping. This file runs the same
//! fixture with the resume switched OFF and asserts the ORIGINAL abort, so the
//! pair is a real A/B: same binary, same fixture, one switch, two outcomes.
//!
//! It is also the thing that keeps the kill switch honest. A default-ON knob
//! whose OFF arm nothing exercises is a knob that quietly stops working, and
//! the next person to reach for it during a bisect finds out the hard way.
//!
//! # Why a separate test binary and not a second `#[test]`
//!
//! `deopt_sink_resume_enabled` is a `OnceLock`, latched on first read and never
//! re-read. Two arms in one process would both see whichever value the first
//! one latched, and the second would pass or fail for a reason that has nothing
//! to do with its own override. One process per arm is the only honest split.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::error::{MethodCallFailed, VmError};
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

const CLASS: &str = "cratonvm/CompiledNpeMessage";
const METHOD: &str = "storeToNull";
const DESC: &str = "(I)I";
const WARM_CALLS: i32 = 3_000;





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
        "{CLASS}.{METHOD} never reached the OPTIMIZING backend — this arm is about what the \
         switch does to an IR body, so proceeding would measure the single-pass backend and \
         pass, or fail, for the wrong reason"
    );
}




