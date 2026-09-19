// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Can the embedding default VM run four `String` calls and a `ServiceLoader`?
//!
//! `VmConfig::default()` is `JdkMode::Synthetic` (`EMBEDDED_DEFAULT_JDK_MODE`),
//! deliberately: the library path stays hermetic so the in-tree suite and
//! embedders that ship no JDK do not resolve JMODs from whatever JDK the build
//! machine happens to have. In a DEFAULT build — no `synthetic-jdk` feature —
//! that leaves a VM with no JDK bytecode AND no synthetic class library: the
//! ~5,200 stubs are `#[cfg(feature = "synthetic-jdk")]` and are not compiled
//! in. What remains is `register_essential_natives_with_shims`.
//!
//! Until 2026-09-05 `vm_init`'s `#[cfg(not(feature = "synthetic-jdk"))]` arm
//! opened with an unconditional `set_drop_real_layout_synthetic(true)`, and
//! `NativeMethodRegistry::register` drops every `java/lang/String` `Bridge`
//! when that is set. So the essentials registered `length()` a few lines later
//! and the registry threw them away — leaving `"abcdef".length()` with no
//! bytecode and no native, raising `java/lang/NoSuchMethodError`. The same
//! shape, by a different mechanism, took `ServiceLoader.load(Class)`: its
//! registrar's body was `#[cfg(feature = "synthetic-jdk")]`, so the 2026-08-29
//! retirement in favour of the JDK's own lazy iterator also removed it in a run
//! that has no such iterator.
//!
//! # Why this file exists rather than the tests that found it
//!
//! `wp7_2_jdbc_core_types_reachable` and `wp1_8_real_jar_serviceloader` are
//! what went red, but both now skip in a default build (`synthetic_library_
//! available()`, 2026-09-05) — correctly, because what they measure past this
//! point *is* a shim. This file is the part that is not a shim question: these
//! five names have a real implementation in the default binary, and nothing in
//! this configuration can shadow it.
//!
//! # The other half of the pair
//!
//! `wp8_10_9_string_contains_native.rs` asserts the inverse in the mode where
//! the inverse is right: in a REAL-JDK registry every `java/lang/String`
//! `Bridge` is absent, so the real bytecode wins. Neither test is meaningful
//! without the other — together they say the policy is keyed on the run, which
//! is the whole content of the fix.

use cratonvm_types::error::MethodCallFailed;
use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

fn resources_dir() -> String {
    format!("{}/tests/resources", env!("CARGO_MANIFEST_DIR"))
}

/// The fixture is checked in, like its neighbours in `tests/resources`, so a
/// box without `javac` still runs this. If it is ever un-staged, say so rather
/// than skipping: a silent skip here is a green run that asserted nothing, and
/// asserting nothing is exactly the failure this file was written about.
fn fixture_staged() -> bool {
    std::path::Path::new(&format!(
        "{}/cratonvm/EssentialSurface.class",
        resources_dir()
    ))
    .exists()
}

fn call(method: &str) -> Result<i32, String> {
    let config = VmConfig::new().with_classpath(vec![resources_dir()]);
    let mut vm = Vm::new(config);
    match vm.invoke("cratonvm/EssentialSurface", method, "()I", &[]) {
        Ok(Some(Value::Int(n))) => Ok(n),
        Ok(other) => Err(format!("returned {other:?}, expected an int")),
        Err(MethodCallFailed::ExceptionThrown(exc)) => {
            let class_id = vm.shared.mem.heap.class_id_of(exc);
            let name = vm
                .shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .map(|c| c.name.to_string())
                .unwrap_or_else(|| format!("<unknown class {class_id:?}>"));
            Err(format!("threw {name}"))
        }
        Err(e) => Err(format!("{e:?}")),
    }
}

#[test]
fn the_embedding_default_vm_can_run_the_essential_string_surface() {
    assert!(
        fixture_staged(),
        "vm/tests/resources/cratonvm/EssentialSurface.class is not staged. It is checked \
         in, so this means it was deleted or the resources directory moved — fix that \
         rather than skipping, because a skip here is a green run that asserted nothing."
    );

    // (method, expected). Each of the four raised `NoSuchMethodError` before
    // 2026-09-05; `charAt` and `substring` are here because they are registered
    // by the same essentials pass and dropped by the same rule, so a fix that
    // restored only `length()` would be the wrong fix passing this test.
    for (method, expected) in [
        ("literalLength", 6),
        ("literalIsEmpty", 1),
        ("literalCharAt", 99),
        ("literalSubstringLength", 3),
    ] {
        match call(method) {
            Ok(n) => assert_eq!(
                n, expected,
                "cratonvm/EssentialSurface.{method}() returned {n}, expected {expected}"
            ),
            Err(e) => panic!(
                "cratonvm/EssentialSurface.{method}() {e}.\n\
                 \n\
                 A `NoSuchMethodError` here means the `java/lang/String` essentials were \
                 dropped from a registry that has nothing else to answer with. Check that \
                 `vm_init`'s `#[cfg(not(feature = \"synthetic-jdk\"))]` arm still guards \
                 `set_drop_real_layout_synthetic(true)` with `if !config.use_synthetic_jdk` \
                 — the drop is correct only when there is real bytecode behind it, and \
                 `VmConfig::default()` is `JdkMode::Synthetic`."
            ),
        }
    }
}

#[test]
fn the_embedding_default_vm_can_load_a_service_loader() {
    assert!(
        fixture_staged(),
        "vm/tests/resources/cratonvm/EssentialSurface.class is not staged; see the sibling \
         test for why this is an assertion and not a skip."
    );

    match call("serviceLoaderLoads") {
        Ok(1) => {}
        Ok(n) => panic!("serviceLoaderLoads() returned {n}, expected 1"),
        Err(e) => panic!(
            "cratonvm/EssentialSurface.serviceLoaderLoads() {e}.\n\
             \n\
             `service_loader::register_service_loader_natives` retires its nine \
             `java/util/ServiceLoader` rows in favour of the JDK's own lazy iterator. That \
             retirement is right, and is keyed on `drops_real_layout_synthetic()` — the \
             RUN's JDK mode. If it is keyed on the Cargo feature again, this configuration \
             has neither the natives nor the bytecode."
        ),
    }
}
