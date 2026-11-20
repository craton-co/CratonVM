// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! `lambda_jit_tierup_tests.rs`, re-run against the OTHER half of the fix.
//!
//! The lambda tier-up has two halves, and they serve different callers. A
//! compiled caller's SAM call is answered by the JIT-side direct arm
//! (`jit::helpers::try_lambda_site_direct_call`); an interpreted caller's is
//! answered by the interpreter's one-shot primitive
//! (`jit_bridge::execute_jit_call_oneshot`). Every fixture method in
//! `LambdaJitTierUp` loops, so every one of them gets its caller OSR-compiled
//! within a few hundred iterations — which means the sibling suite exercises
//! the JIT-side arm and *only* the JIT-side arm.
//!
//! That was measured, not assumed: with a deliberate off-by-one planted in the
//! one-shot's return-value conversion and its implicit-NPE drain deleted, all
//! twelve of those tests still passed.
//!
//! So this file runs the identical fixture, against the identical golden
//! values, with `CRATONVM_JIT_LAMBDA_SITE=0` — which sends a compiled caller's
//! SAM call back down the generic path and therefore through the one-shot.
//! Same numbers, other half. Its own file so the variable is set before any
//! dispatch in the process reads it.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

fn test_resources_dir() -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn class_files_available() -> bool {
    let dir = test_resources_dir();
    std::path::Path::new(&format!("{dir}/cratonvm/LambdaJitTierUp.class")).exists()
}

fn real_jdk_vm() -> Option<Vm> {
    let java_home = cratonvm_vm::config::resolve_java_home_public(None)?;
    let mut config = VmConfig::new().with_classpath(vec![test_resources_dir()]);
    config.use_synthetic_jdk = false;
    Some(Vm::new(
        config.with_java_home(java_home.to_string_lossy().into_owned()),
    ))
}

// Golden values below were produced by running the byte-identical
// `LambdaJitTierUp.java` fixture under a real JDK (Temurin 25.0.3), driver
// calling each `*Checksum()` method reflectively and printing its result.
//
// Several of them overflow `int`, deliberately: 200 000 iterations of a sum is
// what it takes for these to be testing the compiled path rather than the
// interpreter (see the fixture's own note on `N`), and a wrapped sum is a
// perfectly good checksum — it just has to be the SAME wrapped sum a real JDK
// produces, which is what these are.












