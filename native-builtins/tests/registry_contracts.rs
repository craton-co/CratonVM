// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Cross-module contracts for the production native registry.

use cratonvm_native_api::{NativeKind, NativeMethodRegistry};
use cratonvm_native_builtins::register_essential_natives;

fn production_registry() -> NativeMethodRegistry {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);
    registry
}

#[test]
fn essential_security_io_and_concurrency_bridges_are_real() {
    let registry = production_registry();
    let required = [
        ("java/lang/System", "loadLibrary", "(Ljava/lang/String;)V"),
        (
            "java/lang/Runtime",
            "loadLibrary0",
            "(Ljava/lang/Class;Ljava/lang/String;)V",
        ),
        (
            "java/util/concurrent/ForkJoinPool",
            "execute",
            "(Ljava/lang/Runnable;)V",
        ),
        (
            "java/lang/System",
            "arraycopy",
            "(Ljava/lang/Object;ILjava/lang/Object;II)V",
        ),
    ];

    for (class, method, descriptor) in required {
        assert!(
            registry.find(class, method, descriptor).is_some(),
            "missing production native {class}.{method}{descriptor}"
        );
        assert_ne!(
            registry.kind_of(class, method, descriptor),
            Some(NativeKind::SyntheticStub),
            "{class}.{method}{descriptor} must not be an application-visible stub"
        );
    }

    // Production mode deliberately leaves these high-level methods
    // unregistered so the real JDK bytecode executes. Registering the legacy
    // synthetic implementations here would restore the fake object layouts
    // that caused the RAF crash and ForkJoinPool semantic drift.
    for (class, method, descriptor) in
        [("java/io/RandomAccessFile", "read", "([BII)I")]
    {
        assert!(
            registry.find(class, method, descriptor).is_none(),
            "production mode must delegate {class}.{method}{descriptor} to real JDK bytecode"
        );
    }
}

#[test]
fn registry_rows_have_nonempty_method_identity() {
    for (class, method, descriptor, _) in production_registry().dump_registrations() {
        assert!(!class.is_empty(), "native class name must not be empty");
        assert!(!method.is_empty(), "native method name must not be empty");
        assert!(!descriptor.is_empty(), "descriptor must not be empty");
        // The registry also carries field access bridges, whose descriptors
        // are field descriptors rather than method descriptors.
        if descriptor.starts_with('(') {
            assert!(
                descriptor.contains(')'),
                "invalid method descriptor for {class}.{method}: {descriptor}"
            );
        }
    }
}
