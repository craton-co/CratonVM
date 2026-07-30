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

/// A class whose state the VM owns must be intercepted *completely*.
///
/// `StampedLock` is the worked example. Its native backend keeps the lock word
/// in a Rust-side table rather than the real `state` field, so any method left
/// to real JDK bytecode reads and spins on a word nothing maintains. A partial
/// interception is worse than none: with no registration at all the real
/// bytecode is at least self-consistent with itself.
///
/// This is not hypothetical. `tryConvertToOptimisticRead` was the single
/// missing entry, and because Agroal's `StampedCopyOnWriteArrayList` uses it as
/// its release path instead of `unlockWrite`, the call released nothing and
/// deadlocked the Keycloak boot. Nothing in the registry's shape made that one
/// gap visible, which is what this gate is for.
#[test]
fn stamped_lock_surface_is_intercepted_completely() {
    let registry = production_registry();

    const SL: &str = "java/util/concurrent/locks/StampedLock";
    const WRITE_VIEW: &str = "java/util/concurrent/locks/StampedLock$WriteLockView";
    const READ_VIEW: &str = "java/util/concurrent/locks/StampedLock$ReadLockView";

    let required = [
        // Acquire.
        (SL, "<init>", "()V"),
        (SL, "readLock", "()J"),
        (SL, "writeLock", "()J"),
        (SL, "readLockInterruptibly", "()J"),
        (SL, "writeLockInterruptibly", "()J"),
        (SL, "tryReadLock", "()J"),
        (SL, "tryWriteLock", "()J"),
        (SL, "tryReadLock", "(JLjava/util/concurrent/TimeUnit;)J"),
        (SL, "tryWriteLock", "(JLjava/util/concurrent/TimeUnit;)J"),
        (SL, "tryOptimisticRead", "()J"),
        // Release. Each of these is the path some library takes instead of the
        // obvious one.
        (SL, "unlock", "(J)V"),
        (SL, "unlockRead", "(J)V"),
        (SL, "unlockWrite", "(J)V"),
        (SL, "unstampedUnlockRead", "()V"),
        (SL, "unstampedUnlockWrite", "()V"),
        (SL, "tryUnlockRead", "()Z"),
        (SL, "tryUnlockWrite", "()Z"),
        // Conversion — where the Agroal deadlock came from.
        (SL, "tryConvertToReadLock", "(J)J"),
        (SL, "tryConvertToWriteLock", "(J)J"),
        (SL, "tryConvertToOptimisticRead", "(J)J"),
        // Observation. A stale answer here is a silent wrong result rather
        // than a hang, which makes it harder to find, not easier.
        (SL, "validate", "(J)Z"),
        (SL, "isLocked", "()Z"),
        (SL, "isReadLocked", "()Z"),
        (SL, "isWriteLocked", "()Z"),
        (SL, "getReadLockCount", "()I"),
        // The Lock views delegate to the same backend and carry the same
        // all-or-nothing requirement.
        (WRITE_VIEW, "lock", "()V"),
        (WRITE_VIEW, "tryLock", "()Z"),
        (WRITE_VIEW, "unlock", "()V"),
        (READ_VIEW, "lock", "()V"),
        (READ_VIEW, "tryLock", "()Z"),
        (READ_VIEW, "unlock", "()V"),
    ];

    let missing: Vec<String> = required
        .iter()
        .filter(|(class, method, descriptor)| registry.find(class, method, descriptor).is_none())
        .map(|(class, method, descriptor)| format!("{class}.{method}{descriptor}"))
        .collect();

    assert!(
        missing.is_empty(),
        "these StampedLock methods would run real JDK bytecode against a lock \
         word the native backend does not maintain, and hang: {missing:?}"
    );
}
