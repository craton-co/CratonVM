// SPDX-License-Identifier: Apache-2.0
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

#[test]
fn wide_capturing_lambda_enters_compiled_direct_site() {
    let resources = format!("{}/tests/resources", env!("CARGO_MANIFEST_DIR"));
    assert!(
        std::path::Path::new(&format!("{resources}/cratonvm/LambdaWideCaptures.class")).exists(),
        "LambdaWideCaptures fixture must be compiled"
    );
    let java_home = cratonvm_vm::config::resolve_java_home_public(None)
        .expect("wide-lambda acceptance requires a real JDK");
    cratonvm_types::flags::with_process_overrides(
        &[
            ("CRATONVM_DBG_LAMBDA_JIT", Some("1")),
            ("CRATONVM_JIT_LAMBDA_SITE", Some("1")),
            // As in jit_lambda_door_deopt_resumes: keep a small optimizing
            // caller installed instead of letting its economic gate retire it.
            ("CRATONVM_TIER_C1_THRESHOLD", Some("100000")),
            ("CRATONVM_TIER_C2_THRESHOLD", Some("600")),
            ("CRATONVM_TIER_C2_MIN_INVOCATIONS", Some("500")),
            ("CRATONVM_C2_ACCEPT", Some("always")),
        ],
        || {
            let mut config = VmConfig::new().with_classpath(vec![resources]);
            config.use_synthetic_jdk = false;
            let mut vm = Vm::new(config.with_java_home(java_home.to_string_lossy().into_owned()));
            let before = lambda_jit_engagement().1;
            // JIT r22 wave 7 (lane inline): since the framed lambda thunk
            // (`CRATONVM_JIT_LAMBDA_FRAMED_THUNK`, default ON, JIT r20 wave 19,
            // DISP20-2) a SAM site whose impl takes more words than the
            // registers -- this one: nine captures plus the argument -- gets
            // its inline-cache thunk after a few Rust direct calls, and every
            // later call enters the impl's compiled body from machine code
            // WITHOUT passing the Rust direct arm that `site_direct` counts.
            // The test counted only that arm, so it failed on every build
            // since (`direct 3 -> 3`: three direct calls, then the thunk). A
            // capturing thunk installed during the checksum is the stronger
            // proof that no call is interpreted; it is accepted beside the
            // direct-arm count.
            let capture_thunks_before = lambda_jit_capture_adapter_installs();
            let result = vm.invoke("cratonvm/LambdaWideCaptures", "checksum", "()J", &[]);
            let expected: i64 = (0..200_000i64).map(|n| 1020 + 2 * (n & 255)).sum();
            assert_eq!(result.ok().flatten(), Some(Value::Long(expected)));
            require_compiled_caller(&mut vm);
            let thunk_installed = lambda_jit_capture_adapter_installs() > capture_thunks_before;
            let before_throw = lambda_jit_engagement().1;
            let before_outcomes = lambda_site_deopt_outcomes();
            assert_eq!(
                vm.invoke("cratonvm/LambdaWideCaptures", "exceptionOnce", "()I", &[])
                    .ok()
                    .flatten(),
                Some(Value::Int(1)),
                "the wide compiled throw must propagate without replaying its side effect"
            );
            let after_throw = lambda_jit_engagement().1;
            let after_outcomes = lambda_site_deopt_outcomes();
            eprintln!(
                "wide throw: direct {} -> {}, resumed {} -> {}, unresumable {} -> {}",
                before_throw,
                after_throw,
                before_outcomes.0,
                after_outcomes.0,
                before_outcomes.1,
                after_outcomes.1
            );
            assert!(
                after_throw > before_throw || after_outcomes.0 > before_outcomes.0 || thunk_installed,
                "the throw must enter the warmed compiled direct site (or its installed thunk); \
                 a cold interpreter caller is insufficient"
            );
            assert_eq!(
                after_outcomes.1, before_outcomes.1,
                "wide throw must not leave an unresumable compiled frame"
            );
            let after = after_throw;
            assert!(
                thunk_installed || after.saturating_sub(before) > 100_000,
                "only {} of 400000 calls entered the compiled direct site; a stale narrow buffer silently interprets them",
                after.saturating_sub(before)
            );
        },
    );
}

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
