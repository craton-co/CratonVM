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
            // JIT r23 wave 8 (lane W8-L2): `checksum`'s 200 000-iteration loop
            // may be OSR-compiled by the optimizing tier with `make`,
            // `applyTo` and the lambda body all spliced in and the capturing
            // lambda allocated inline (`[ir] indy CAPTURE NEW ... allocated
            // inline`, `spliced 20 callee bodies into ...checksum()J`). That
            // body is correct: the checksum and the exception check both pass.
            // It is also faster (the failing runs finished in ~1.3 s, the
            // passing ones in 20-35 s), but its calls never reach the one-shot
            // door, so the engagement count this test is about reads a few
            // hundred. Whether that OSR body is ready in time depends on
            // background-compile timing (wave-8 full `cargo test` runs hit it,
            // isolated runs about one time in eight under
            // `CRATONVM_DBG=jit-compiled,ir-compiles`). OSR is pinned off so the
            // measured calls always go through `applyTo`'s call site, which is
            // the door the stale narrow buffer used to bypass.
            ("CRATONVM_JIT_OSR", Some("0")),
            // JIT r23 wave 9 (lane W9-L2): the second bypass. `applyTo`'s own
            // optimizing compile may splice the lambda body at its `apply`
            // site (`CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE`, default ON:
            // `inline-plan ...applyTo: 1 site(s), 1 spliced body` and
            // `acceptance ...applyTo: accepted (evidence: inlined)`). That is
            // also a correct, faster body whose calls never reach the one-shot
            // door. Whether it is accepted depends on the profile at compile
            // time, so it is order- and load-dependent: on the w9b test build,
            // under a 24-thread CPU hog, 1 of 3 runs failed in 19 s with
            // "only 210 of 400000" and exactly that accepted splice; with this
            // pin, 4 of 4 loaded runs passed and `applyTo` spliced nothing.
            // The spliced path is covered by the lambda-SAM splice tests; this
            // test is about the wide one-shot door, so the splice is pinned off.
            ("CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE", Some("0")),
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
