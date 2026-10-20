// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The Hibernate cascade lambda has ten Java arguments. The one-shot door
//! supports the wide ABI, but its caller formerly declined at a stale 8-word
//! buffer before entering that door. Correct values alone cannot detect this:
//! assert compiled one-shot engagement as well.
use cratonvm_vm::config::VmConfig;
use cratonvm_vm::runtime::interpreter::lambda_jit_engagement;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

#[test]
fn wide_capturing_lambda_enters_compiled_oneshot() {
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
            ("CRATONVM_JIT_LAMBDA_SITE", Some("0")),
        ],
        || {
            let mut config = VmConfig::new().with_classpath(vec![resources]);
            config.use_synthetic_jdk = false;
            let mut vm = Vm::new(config.with_java_home(java_home.to_string_lossy().into_owned()));
            let before = lambda_jit_engagement().0;
            let result = vm.invoke("cratonvm/LambdaWideCaptures", "checksum", "()J", &[]);
            let expected: i64 = (0..200_000i64).map(|n| 1020 + 2 * (n & 255)).sum();
            assert_eq!(result.ok().flatten(), Some(Value::Long(expected)));
            assert_eq!(
                vm.invoke("cratonvm/LambdaWideCaptures", "exceptionOnce", "()I", &[])
                    .ok()
                    .flatten(),
                Some(Value::Int(1)),
                "the wide compiled throw must propagate without replaying its side effect"
            );
            let after = lambda_jit_engagement().0;
            assert!(
                after.saturating_sub(before) > 100_000,
                "only {} of 400000 calls entered the compiled one-shot; a stale narrow buffer silently interprets them",
                after.saturating_sub(before)
            );
        },
    );
}
