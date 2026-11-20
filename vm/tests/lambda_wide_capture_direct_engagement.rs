// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The Hibernate cascade lambda has ten Java arguments. The direct site door
//! supports the wide ABI, but its caller formerly declined at a stale 8-word
//! buffer before entering that door. Correct values alone cannot detect this:
//! assert compiled direct site engagement as well.
use cratonvm_vm::config::VmConfig;
use cratonvm_vm::runtime::interpreter::{
    lambda_jit_capture_adapter_installs, lambda_jit_engagement, lambda_site_deopt_outcomes,
};
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;


// A hot SAM body does not prove that the caller used by exceptionOnce is
// compiled: checksum can inline applyTo in its OSR body while applyTo itself
// never gets an entry artifact. Check the exact caller before measuring it.
fn require_compiled_caller(vm: &mut Vm) {
    const CLASS: &str = "cratonvm/LambdaWideCaptures";
    for _ in 0..400 {
        let class_id = vm
            .shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(CLASS)
            .expect("wide fixture must be loaded");
        if vm
            .shared
            .jit
            .jit_cache
            .read()
            .get(
                CLASS,
                "applyTo",
                "(Ljava/util/function/Function;I)I",
                class_id,
            )
            .is_some()
        {
            return;
        }
        for _ in 0..50 {
            vm.invoke(CLASS, "warmCaller", "(I)I", &[Value::Int(64)])
                .expect("nonthrowing Java caller warm-up must succeed");
        }
    }
    panic!("applyTo must have a compiled entry before measuring the wide lambda throw");
}
