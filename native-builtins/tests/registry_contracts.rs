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

/// Every path that can resolve a native symbol or load a native library must
/// pass the host-native-access gate.
///
/// This is a source-level scan rather than a behavioural test because the hole
/// it exists to catch is an *omission*: four separate natives
/// (`NativeLibrary.findEntry0`, `SymbolLookup.loaderLookup`, `SymbolLookup.find`
/// and `JavaLangAccess.findNative`) reached `find_native_symbol` without ever
/// calling a gate, so the load side was closed while the read side stayed open.
/// A guest could hand `findEntry0` a small integer handle — it is just
/// `lib_index + 1` — and walk it to `dlsym` any library loaded by trusted code.
/// Nothing failed, because there was no test that could fail: each native was
/// individually plausible, and only the *set* was wrong.
///
/// So the gate is all-or-nothing over the set, the same shape as
/// `stamped_lock_surface_is_intercepted_completely` above. A new native that
/// reaches either capability fails this test until it is gated or explicitly
/// listed as exempt.
#[test]
fn native_symbol_lookups_all_pass_the_host_access_gate() {
    const GATES: [&str; 2] = ["check_host_native_access_or_throw", "require_native_access"];
    const CAPABILITIES: [&str; 2] = ["find_native_symbol", "load_native_library"];

    // Exempt: reached only from VM-internal bootstrap, never from a Java
    // caller's control. Keep this list short and justified.
    const EXEMPT_FNS: [&str; 0] = [];

    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut ungated: Vec<String> = Vec::new();
    let mut scanned_files = 0usize;
    let mut call_sites = 0usize;

    let mut stack = vec![src_dir.clone()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).expect("native-builtins/src must be readable");
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            scanned_files += 1;

            // Everything from the first `#[cfg(test)]` module onward is test
            // scaffolding, which legitimately calls these directly.
            let body = match text.find("#[cfg(test)]") {
                Some(cut) => &text[..cut],
                None => &text[..],
            };

            for cap in CAPABILITIES {
                let needle = format!("ctx.{cap}(");
                let mut from = 0usize;
                while let Some(rel) = body[from..].find(&needle) {
                    let at = from + rel;
                    from = at + needle.len();

                    // Skip commented-out mentions.
                    let line_start = body[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
                    let prefix = body[line_start..at].trim_start();
                    if prefix.starts_with("//") || prefix.starts_with("///") {
                        continue;
                    }
                    call_sites += 1;

                    // Scope: the enclosing registered closure, or failing that
                    // the enclosing free function.
                    let scope_start = body[..at]
                        .rfind("r.register(")
                        .or_else(|| body[..at].rfind("\nfn "))
                        .unwrap_or(0);
                    let scope = &body[scope_start..at];
                    if EXEMPT_FNS.iter().any(|f| scope.contains(f)) {
                        continue;
                    }
                    if GATES.iter().any(|g| scope.contains(g)) {
                        continue;
                    }
                    let line = body[..at].matches('\n').count() + 1;
                    let file = path.strip_prefix(&src_dir).unwrap_or(&path).display();
                    ungated.push(format!("{file}:{line} ({cap})"));
                }
            }
        }
    }

    // Vacuity guard: a scan that silently stops finding call sites would
    // "pass" forever. This is the failure mode the audit that produced this
    // test was written about.
    assert!(
        scanned_files > 10,
        "source scan found only {scanned_files} files — the scan itself is broken"
    );
    assert!(
        call_sites >= 8,
        "source scan found only {call_sites} native-symbol call sites; expected \
         at least 8. Either the capability was renamed or the scan is broken."
    );

    assert!(
        ungated.is_empty(),
        "these natives resolve a native symbol or load a native library without \
         passing the host-native-access gate, so a caller denied `loadLibrary.*` \
         (or running under CRATONVM_UNTRUSTED_CODE) can still reach them: {ungated:?}"
    );
}
