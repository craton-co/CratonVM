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

/// The WatchService surface must have EXACTLY ONE owner.
///
/// `java.nio.file.WatchService`/`WatchKey` are interfaces, so every object the
/// surface hands out is a synthetic carrier with a hand-rolled slot layout —
/// and CratonVM once had two independent implementations of that surface with
/// two incompatible layouts. Registration is last-write-wins, so which one ran
/// depended purely on the order the arms in `vm/src/vm/vm_init.rs` happened to
/// call their `register_*` functions. The real-JDK arm calls
/// `register_io_natives` and THEN `register_phase57_nio_file`, which is how a
/// placeholder `FileSystem.newWatchService` (a bare object with ZERO fields)
/// displaced the notify-backed implementation: `Path.register` then reported
/// "service is closed or unknown" and `WatchService.close` wrote past the
/// receiver's layout.
///
/// This test replays that exact order and asserts the outcome, rather than
/// merely asserting the natives are present — a presence check passes just as
/// happily when the loser is the one left standing.
#[test]
fn watch_service_surface_has_a_single_owner_in_native_io() {
    let mut registry = NativeMethodRegistry::new();
    // Real-JDK arm ordering, `vm/src/vm/vm_init.rs`.
    cratonvm_native_io::register_io_natives(&mut registry);
    cratonvm_native_builtins::phases_late::register_phase57_nio_file(&mut registry);

    // The timed overload is the one a watch loop actually calls; when it was
    // missing the call fell through to the interface's abstract declaration
    // and threw `AbstractMethodError: ... has no Code attribute`.
    let surface = [
        (
            "java/nio/file/FileSystem",
            "newWatchService",
            "()Ljava/nio/file/WatchService;",
        ),
        (
            "java/nio/file/Path",
            "register",
            "(Ljava/nio/file/WatchService;[Ljava/nio/file/WatchEvent$Kind;)Ljava/nio/file/WatchKey;",
        ),
        (
            "java/nio/file/WatchService",
            "poll",
            "()Ljava/nio/file/WatchKey;",
        ),
        (
            "java/nio/file/WatchService",
            "poll",
            "(JLjava/util/concurrent/TimeUnit;)Ljava/nio/file/WatchKey;",
        ),
        (
            "java/nio/file/WatchService",
            "take",
            "()Ljava/nio/file/WatchKey;",
        ),
        ("java/nio/file/WatchService", "close", "()V"),
        ("java/nio/file/WatchKey", "pollEvents", "()Ljava/util/List;"),
        ("java/nio/file/WatchKey", "reset", "()Z"),
        ("java/nio/file/WatchKey", "cancel", "()V"),
        ("java/nio/file/WatchKey", "isValid", "()Z"),
        (
            "java/nio/file/WatchKey",
            "watchable",
            "()Ljava/nio/file/Watchable;",
        ),
        (
            "java/nio/file/WatchEvent",
            "kind",
            "()Ljava/nio/file/WatchEvent$Kind;",
        ),
        (
            "java/nio/file/WatchEvent",
            "context",
            "()Ljava/lang/Object;",
        ),
    ];

    let census = registry.census();
    for (class, name, descriptor) in surface {
        assert!(
            registry.find(class, name, descriptor).is_some(),
            "WatchService surface is missing {class}.{name}{descriptor}"
        );
        let owners: Vec<&str> = census
            .iter()
            .filter(|e| e.class == class && e.name == name && e.descriptor == descriptor)
            .map(|e| e.registered_by.as_deref().unwrap_or("<unknown site>"))
            .collect();
        assert_eq!(
            owners.len(),
            1,
            "{class}.{name}{descriptor} is registered {n} times, by {owners:?} — two \
             implementations of one triple means two incompatible synthetic layouts, \
             and which one wins is decided purely by registration order",
            n = owners.len()
        );
        assert!(
            owners[0].contains("native-io"),
            "{class}.{name}{descriptor} is owned by {owner} — the notify-backed \
             implementation in `cratonvm-native-io` must be the owner",
            owner = owners[0]
        );
    }
}

/// `javax/net/ssl/SSLSocketFactory.getDefault()` must be registered EXACTLY once.
///
/// It was registered twice: the correct implementation in
/// `phases_late::ssl_security::register_p68_ssl`, which resolves the runtime
/// default `SSLContext` and stores it at field 0, and a stale twin at the tail
/// of `net_phase_e::register_re6_ssl_context` that stored
/// `Value::Object(None)` there instead. The registry is last-registration-wins
/// for a repeated triple, and `register_phase_e_networking` runs after
/// `register_p68_ssl` (`lib.rs:17505` then `:17528`), so the broken twin won on
/// every boot.
///
/// That made the 2026-07-23 fix present, correct and DEAD — re-reading
/// `ssl_security.rs` showed the right code while every caller of the static
/// `getDefault()` still received a factory with no owning context, so the
/// layered `createSocket(Socket,String,int,boolean)` overload threw
/// `IllegalStateException("SSLSocketFactory has no owning SSLContext")` and
/// Aether/Maven resolution failed under every `ModifiedClassPathClassLoader`
/// test. See `docs/known-issues/springboot/
/// sslsocketfactory-getdefault-aether-resolution-regression-20260804.md`.
///
/// Asserting over `dump_registrations()` rather than `find(...)` is the entire
/// point of this test: `find` returns the winner, so it reports a healthy
/// registry whether the triple was registered once or five times. Only the
/// registration list can see the duplicate — which is why a bug that was
/// diagnosed, documented and "fixed" still shipped.
#[test]
fn ssl_socket_factory_get_default_is_registered_exactly_once() {
    let registry = production_registry();
    let (class, method, descriptor) = (
        "javax/net/ssl/SSLSocketFactory",
        "getDefault",
        "()Ljavax/net/SocketFactory;",
    );

    let sites: Vec<_> = registry
        .dump_registrations()
        .into_iter()
        .filter(|(c, m, d, _)| *c == class && *m == method && *d == descriptor)
        .collect();

    assert_eq!(
        sites.len(),
        1,
        "{class}.{method}{descriptor} must be registered exactly once — a second \
         registration silently replaces the first via last-registration-wins, and \
         `find()` cannot tell the difference. Registrations seen: {sites:?}"
    );

    // The surviving registration must be a usable one, not merely unique: the
    // whole failure mode was a registration that existed and answered calls
    // while handing back a factory with no owning `SSLContext`.
    assert!(
        registry.find(class, method, descriptor).is_some(),
        "{class}.{method}{descriptor} must still be registered at all"
    );
}
