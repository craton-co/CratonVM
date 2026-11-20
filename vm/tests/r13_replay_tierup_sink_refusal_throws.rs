// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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




