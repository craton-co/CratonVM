// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Cross-module contracts for the production native registry.

use cratonvm_native_api::{NativeKind, NativeMethodRegistry};
use cratonvm_native_builtins::register_essential_natives;
use cratonvm_types::compat::CompatibilityMode;

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
    {
        let (class, method, descriptor) = ("java/io/RandomAccessFile", "read", "([BII)I");
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
///
/// Blank out every `#[cfg(test)]` module in `text`, preserving line count.
///
/// Test scaffolding calls the capability helpers directly and must not be
/// scanned, but *truncating* at the first `#[cfg(test)]` throws away all the
/// production code that follows it — which in this crate is most of the code,
/// because the largest files put a test module a few thousand lines in and then
/// carry on for tens of thousands more. Each `#[cfg(test)]` region is replaced
/// by newlines instead, so the scan sees every production line and the line
/// numbers it reports still match the file.
///
/// Region detection matches this crate's layout: a column-0 test `cfg`
/// attribute whose next non-attribute line declares a module, closed by a `}`
/// in column 0. The module check matters — `#[cfg(test)] use ...;` is common,
/// and treating that as the start of a region would swallow production code all
/// the way to the next column-0 brace, i.e. hide call sites. A test module
/// nested inside another item is not excised; that can only produce a false
/// positive (a reported site that is really test code), never a false negative,
/// which is the safe direction for a security gate.
fn strip_test_modules(text: &str) -> String {
    fn is_test_cfg(line: &str) -> bool {
        line.starts_with("#[cfg(test)]") || line.starts_with("#[cfg(all(test")
    }
    fn declares_mod(line: &str) -> bool {
        let l = line.trim_start();
        l.starts_with("mod ") || l.starts_with("pub mod ") || l.starts_with("pub(crate) mod ")
    }

    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < lines.len() {
        let line = lines[i];
        // Look past any further attributes to find what this cfg applies to.
        let opens_test_mod = is_test_cfg(line) && {
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim_start().starts_with("#[") {
                j += 1;
            }
            j < lines.len() && declares_mod(lines[j])
        };
        if !opens_test_mod {
            out.push_str(line);
            i += 1;
            continue;
        }
        // Blank the region through the closing column-0 `}`, keeping newlines
        // so the line numbers this test reports still match the file.
        while i < lines.len() {
            let l = lines[i];
            if l.ends_with('\n') {
                out.push('\n');
            }
            i += 1;
            if l.starts_with('}') {
                break;
            }
        }
    }
    out
}

#[test]
fn native_symbol_lookups_all_pass_the_host_access_gate() {
    const GATES: [&str; 2] = ["check_host_native_access_or_throw", "require_native_access"];
    const CAPABILITIES: [&str; 2] = ["find_native_symbol", "load_native_library"];

    // Exempt: reached only from VM-internal bootstrap, or a private helper
    // every caller of which gates first. Keep this list short and justified —
    // and pair each entry with a check that its justification still holds, or
    // the exemption becomes the hole. See `gated_helper_callers_all_gate`.
    //
    //  * `load_library_or_throw` — `lang_system.rs`. Private to that module and
    //    called from exactly the four `System.load`/`loadLibrary` /
    //    `Runtime.load`/`loadLibrary` closures, each of which calls
    //    `check_host_native_access_or_throw` on the line before. The gate is in
    //    the caller, so the scan's enclosing-scope heuristic cannot see it.
    const EXEMPT_FNS: [&str; 1] = ["load_library_or_throw"];

    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut ungated: Vec<String> = Vec::new();
    let mut scanned_files = 0usize;
    let mut call_sites = 0usize;
    // Scan-integrity accounting: how much of the crate the scan actually looked
    // at, after test modules were blanked out. See the assertion below.
    let mut total_bytes = 0usize;
    let mut production_bytes = 0usize;

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

            // Test scaffolding legitimately calls these directly, so the
            // `#[cfg(test)]` modules are blanked out. They are *excised*, not
            // truncated at: this used to be `text.find("#[cfg(test)]")` +
            // `&text[..cut]`, which stopped scanning at the FIRST test module
            // and so made everything after it invisible. In `lib.rs` the first
            // one sits at line 2596 of 42,525 — the scan was blind to 94% of
            // the crate's largest file, including a real `load_native_library`
            // call site, and an ungated one added there could never have failed
            // this test. The vacuity guard below is what caught it.
            let blanked = strip_test_modules(&text);
            let body: &str = &blanked;
            total_bytes += text.len();
            production_bytes += body.bytes().filter(|b| *b != b'\n').count();

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
    // The scan must actually look at the crate. The previous truncate-at-first-
    // `#[cfg(test)]` implementation retained 6% of `lib.rs`; this catches that
    // class of blindness directly, rather than inferring it from a call-site
    // count that also moves for legitimate reasons.
    assert!(
        production_bytes * 4 >= total_bytes,
        "the scan retained only {production_bytes} of {total_bytes} bytes after \
         blanking test modules — `strip_test_modules` is eating production code, \
         so an ungated call site could hide in the part it never looked at."
    );
    // Vacuity floor. The count is 7 as this is written (`lang_system.rs`,
    // `lib.rs`, four in `panama.rs`, `shared_secrets_bridge.rs`); the floor sits
    // just under it so that removing one native is not a CI failure while the
    // capability being renamed out from under the scan still is. Do not raise
    // this to track the exact count — that is what made it stale before.
    assert!(
        call_sites >= 6,
        "source scan found only {call_sites} native-symbol call sites; expected \
         at least 6. Either the capability was renamed or the scan is broken."
    );

    assert!(
        ungated.is_empty(),
        "these natives resolve a native symbol or load a native library without \
         passing the host-native-access gate, so a caller denied `loadLibrary.*` \
         (or running under CRATONVM_UNTRUSTED_CODE) can still reach them: {ungated:?}"
    );
}

/// The `EXEMPT_FNS` entries above are exemptions from the gate scan, so each one
/// has to keep earning it. This is the companion check for the only entry:
/// `load_library_or_throw` is exempt *because* every caller gates first, so if a
/// fifth caller is added without a gate the exemption silently becomes the hole
/// the scan exists to prevent.
///
/// Scoped to the one module that owns the helper — it is private to
/// `lang_system.rs`, and the test fails if it stops being private, because then
/// callers could appear anywhere.
#[test]
fn exempt_helper_callers_all_gate_first() {
    const HELPER: &str = "load_library_or_throw";
    const GATE: &str = "check_host_native_access_or_throw";

    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lang_system.rs");
    let text = std::fs::read_to_string(&src).expect("lang_system.rs must be readable");
    let body = strip_test_modules(&text);

    assert!(
        body.contains(&format!("fn {HELPER}(")),
        "{HELPER} is no longer defined in lang_system.rs — either it moved (update \
         this test) or it is gone (drop it from EXEMPT_FNS)."
    );
    assert!(
        !body.contains(&format!("pub fn {HELPER}("))
            && !body.contains(&format!("pub(crate) fn {HELPER}(")),
        "{HELPER} became visible outside lang_system.rs, so this test no longer \
         sees all of its callers. Gate it internally instead of exempting it."
    );

    let mut ungated: Vec<usize> = Vec::new();
    let mut calls = 0usize;
    let mut from = 0usize;
    let needle = format!("{HELPER}(");
    while let Some(rel) = body[from..].find(&needle) {
        let at = from + rel;
        from = at + needle.len();
        // WHITESPACE-TOLERANT, and that is the whole point of this loop.
        //
        // This used to look for the literal `load_library_or_throw(ctx`, with a
        // comment claiming the newline in the DEFINITION (`fn
        // load_library_or_throw(\n    ctx:`) was what kept the definition out of
        // the count. Then the call sites grew a fifth argument, rustfmt wrapped
        // seven of the eight the same way, and the needle stopped matching them
        // too: the scan found ONE call and checked the gate on ONE call, which
        // is a detector that had quietly stopped detecting. It only became
        // visible when the count fell under the `>= 4` floor below — i.e. the
        // floor caught it, the check did not.
        //
        // So: match the call by name, skip whitespace, and require the first
        // argument to be `ctx`. The definition is excluded explicitly, by its
        // `fn ` prefix, rather than by relying on how rustfmt happens to wrap
        // it today.
        let is_definition = body[..at].trim_end().ends_with("fn");
        if is_definition {
            continue;
        }
        let arg = body[from..].trim_start();
        if !(arg.starts_with("ctx,") || arg.starts_with("ctx)") || arg.starts_with("ctx ")) {
            continue;
        }
        calls += 1;
        // Look back to the start of the enclosing registered closure.
        let scope_start = body[..at]
            .rfind("register")
            .or_else(|| body[..at].rfind("\nfn "))
            .unwrap_or(0);
        if !body[scope_start..at].contains(GATE) {
            ungated.push(body[..at].matches('\n').count() + 1);
        }
    }

    assert!(
        calls >= 4,
        "expected at least the four System.load/loadLibrary + Runtime.load/\
         loadLibrary call sites, found {calls} — the scan or the helper changed."
    );
    assert!(
        ungated.is_empty(),
        "these {HELPER} call sites do not call {GATE} first, so the EXEMPT_FNS \
         entry for it is no longer true: lang_system.rs lines {ungated:?}"
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

/// Whoever ends up owning a `javax.net.ssl` entry point is decided purely by
/// registration order, and nothing about a duplicate is visible at the call
/// site — which is how the same defect has now shipped three times.
///
/// * 2026-07-23: `SSLSocketFactory.getDefault()` minted a bare 0-field
///   carrier, so the layered `createSocket(Socket,String,int,boolean)`
///   overload — which reads the owning `SSLContext` out of field 0 — threw
///   `IllegalStateException: SSLSocketFactory has no owning SSLContext`.
///   Fixed in `ssl_security.rs`.
/// * 2026-08-04: the identical exception came back VM-wide. The
///   `ssl_security.rs` fix was still present and still correct; a second,
///   never-updated registration of the exact same triple in
///   `net_phase_e.rs` ran later and won by last-registration-wins, restoring
///   the pre-fix `field 0 = None` shape. Every Spring Boot test going
///   through `ModifiedClassPathClassLoader` (Aether resolving
///   `@ClassPathOverrides` coordinates over HTTPS) failed on it. See
///   `sslsocketfactory-getdefault-aether-resolution-regression-20260804-FIXED.md`.
///
/// So this pins the *surviving owner site*, not merely the count: a duplicate
/// that changes who wins is the failure mode, and a triple that is legitimately
/// registered twice (`SSLContext.getDefault`, `SSLContext.getSocketFactory` —
/// both documented in `net_phase_e::register_re6_ssl_context` as deliberately
/// relying on the ordering) must keep the owner it documents.
///
/// Deliberately asserted against the file that must OWN the slot rather than
/// against a registration count, because both answers are needed and only this
/// one survives a legitimate future duplicate being added.
#[test]
fn ssl_entry_points_keep_their_documented_owning_registration() {
    let registry = production_registry();
    let census = registry.census();

    // (class, method, descriptor, owning source file, why)
    let expected: [(&str, &str, &str, &str, &str); 3] = [
        (
            "javax/net/ssl/SSLSocketFactory",
            "getDefault",
            "()Ljavax/net/SocketFactory;",
            "native-builtins/src/phases_late/ssl_security.rs",
            "must return a carrier whose field 0 is the process default \
             SSLContext, or the layered createSocket overload cannot connect",
        ),
        (
            "javax/net/ssl/SSLContext",
            "getDefault",
            "()Ljavax/net/ssl/SSLContext;",
            "native-builtins/src/net_phase_e.rs",
            "the re6 implementation honours setDefault()'s installed context; \
             its duplicate of the ssl_security.rs registration is intentional \
             and documented in register_re6_ssl_context",
        ),
        (
            "javax/net/ssl/SSLContext",
            "getSocketFactory",
            "()Ljavax/net/ssl/SSLSocketFactory;",
            "native-builtins/src/net_phase_e.rs",
            "the re6 implementation stashes the receiving SSLContext at the \
             factory's field 0",
        ),
    ];

    for (class, method, descriptor, owner_file, why) in expected {
        let rows: Vec<&str> = census
            .iter()
            .filter(|e| e.class == class && e.name == method && e.descriptor == descriptor)
            .map(|e| e.registered_by.as_deref().unwrap_or("<unknown site>"))
            .collect();
        assert!(
            !rows.is_empty(),
            "{class}.{method}{descriptor} is not registered at all — {why}"
        );
        // `census()` yields one row per registration, in registration order,
        // so the LAST row is the one that owns the slot (`register` is
        // last-write-wins on the exact triple).
        let owner = rows[rows.len() - 1];
        // `registered_by` comes from `Location::caller()`, whose `file()` uses
        // the HOST path separator — so on Windows every row reads
        // `native-builtins\src\...` and no comparison against a `/`-spelled
        // expectation can ever match. This guard was therefore red on Windows
        // for reasons having nothing to do with what it guards: it reported
        // "owned by ...ssl_security.rs:1521, not by ...ssl_security.rs" —
        // the same file, spelled two ways — while the invariant it exists to
        // protect (registered exactly once, by that file) was intact.
        //
        // Normalize both sides. A guard that cannot pass on a platform is not
        // a guard there; it is a permanently-red test that trains people to
        // ignore this suite, which is exactly what it was written to prevent.
        let norm = |s: &str| s.replace('\\', "/");
        assert!(
            norm(owner).starts_with(&norm(owner_file)),
            "{class}.{method}{descriptor} is owned by {owner}, not by {owner_file}. \
             It is registered {n} time(s), by {rows:?}. Registration order alone \
             decides the winner, so a new or moved registration silently replaced \
             the intended one — {why}",
            n = rows.len()
        );
    }
}

/// No `javax.net.ssl` native may hand back a `SSLSocketFactory` carrier that
/// was allocated with fewer than one field: field 0 is where every consumer
/// looks for the owning `SSLContext`.
///
/// A census cannot see a callback's body, so this is a source-level scan — and
/// per `docs/`, a source-scanning guard is only worth having if an injected
/// violation actually fails it. Injecting
/// `alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocketFactory", 0)` into
/// any scanned file must turn this red; the found-count floor below is what
/// keeps the scan from passing vacuously when a file is renamed or split.
#[test]
fn no_native_mints_a_field_less_ssl_socket_factory_carrier() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let scanned = [
        "src/t27_tls.rs",
        "src/net_phase_e.rs",
        "src/phases_late/ssl_security.rs",
        "src/tls.rs",
    ];

    let mut alloc_sites = 0usize;
    let mut violations: Vec<String> = Vec::new();
    for rel in scanned {
        let path = root.join(rel);
        let src = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "scanned file {rel} is unreadable: {e} — \
                 if it moved, update this list; a silently-skipped file makes \
                 the whole guard vacuous"
            )
        });
        for (i, line) in src.lines().enumerate() {
            let Some(rest) = line.split("\"javax/net/ssl/SSLSocketFactory\"").nth(1) else {
                continue;
            };
            if !line.contains("alloc_concurrent_synthetic") {
                continue;
            }
            alloc_sites += 1;
            // `..., "javax/net/ssl/SSLSocketFactory", 0)` — the field count is
            // the next argument.
            let count = rest.trim_start_matches(&[',', ' '][..]);
            if count.starts_with('0') {
                violations.push(format!("{rel}:{}: {}", i + 1, line.trim()));
            }
        }
    }

    assert!(
        alloc_sites >= 4,
        "found only {alloc_sites} SSLSocketFactory allocation site(s) across \
         {scanned:?} — the scan is not reaching the code it is meant to guard \
         (files renamed or split?), so it would pass vacuously"
    );
    assert!(
        violations.is_empty(),
        "these natives allocate a 0-field javax/net/ssl/SSLSocketFactory \
         carrier; field 0 must hold the owning SSLContext or the layered \
         createSocket(Socket,String,int,boolean) overload cannot connect \
         (use t27_tls::default_ssl_socket_factory_obj):\n  {}",
        violations.join("\n  ")
    );
}

/// Phase 61 must not take ownership of any `java.nio.file.Path` native away
/// from phase 57.
///
/// `NativeMethodRegistry::register` overwrites the slot in place, so the LAST
/// registration of a triple is the one that runs, and
/// `register_synthetic_overrides` calls phase 57 and then phase 61.
/// `register_p61_files_path` used to re-register six Path methods that phase 57
/// already owned; the `resolve` pair among them joined with
/// `std::path::Path::join` and stored the result without going through
/// `p57_alloc_path`, so they skipped the normalize-at-construction step and
/// silently re-introduced the trailing-separator defect from
/// `resourcestests-trailing-slash-path-normalization-FIXED-20260804.md`.
///
/// The assertion compares the WINNING registration site against the site that
/// wins when phase 57 registers alone, rather than asserting a registration
/// merely exists — a presence check passes just as happily when a later phase
/// has displaced the good implementation. Line numbers are compared between two
/// runtime observations, so ordinary edits to the file do not move the goalposts.
///
/// Gated on the feature because `register_synthetic_overrides` — the only
/// caller that puts phase 61 after phase 57, and therefore the only place the
/// displacement can happen — does not exist without it. Ungated, this whole
/// test TARGET fails to compile in the default configuration
/// (`cargo test -p cratonvm-native-builtins`), taking every other contract in
/// this file down with it. The guard therefore runs in the
/// `experimental-features` job, which is also the only place the code it
/// guards is reachable: real-JDK mode reaches phase 57 straight from
/// `vm_init.rs` and never calls phase 61.
#[cfg(feature = "synthetic-jdk")]
#[test]
fn p61_does_not_displace_phase57_path_natives() {
    fn winner(registry: &NativeMethodRegistry, name: &str, descriptor: &str) -> Option<String> {
        // `census()` is sorted by (class, name, descriptor) with a stable sort,
        // so duplicate triples stay in registration order: the last row is the
        // live owner.
        registry
            .census()
            .into_iter()
            .filter(|e| {
                e.class == "java/nio/file/Path" && e.name == name && e.descriptor == descriptor
            })
            .next_back()
            .and_then(|e| e.registered_by)
    }

    // These six are the ones phase 61 used to duplicate. `getParent` and
    // `getNameCount` had already been hand-synced to their phase-57 twins,
    // which is exactly the drift hazard this test removes.
    let surface = [
        ("getFileName", "()Ljava/nio/file/Path;"),
        ("getParent", "()Ljava/nio/file/Path;"),
        ("toAbsolutePath", "()Ljava/nio/file/Path;"),
        ("resolve", "(Ljava/lang/String;)Ljava/nio/file/Path;"),
        ("resolve", "(Ljava/nio/file/Path;)Ljava/nio/file/Path;"),
        ("getNameCount", "()I"),
    ];

    let mut phase57_only = NativeMethodRegistry::new();
    cratonvm_native_builtins::phases_late::register_phase57_nio_file(&mut phase57_only);

    let mut full = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_synthetic_overrides(&mut full);

    for (name, descriptor) in surface {
        let expected = winner(&phase57_only, name, descriptor);
        assert!(
            expected.is_some(),
            "phase 57 no longer registers java/nio/file/Path.{name}{descriptor} — \
             this test has nothing left to guard and would pass vacuously"
        );
        assert_eq!(
            winner(&full, name, descriptor),
            expected,
            "java/nio/file/Path.{name}{descriptor} is owned by a LATER registration \
             than phase 57's. A duplicate registration of a Path native silently \
             replaces the phase-57 implementation (last write wins); delete it \
             instead of keeping a copy in sync."
        );
    }
}

/// The `--jdk-only` `System.getProperties` arm fills the REAL `map` field, and
/// `native-api`'s Phase 3 retirement table is why this is a test.
///
/// `RETIRED_SHADOW_PHASE3_TRIPLES` retires `java/util/Properties.getProperty`
/// and 32 of its siblings. JDK 9 moved `Properties`' storage into a
/// `ConcurrentHashMap` field named `map`, so every one of those real bodies
/// reads that field, and the object `System.getProperties()` returns is built
/// by this VM. Between G60-1 and 2026-09-09 the field was PERMANENTLY NULL and
/// `retired_shadow.rs` held both `getProperty` overloads back for exactly that
/// reason, in these words: *retire them in the same change that makes that
/// receiver real, or not at all.*
///
/// `native-api` cannot check the other half of that bargain — `native-builtins`
/// depends on it and not the reverse — so a comment there is all it has, and a
/// comment is not a compile-time link. This is the link. Delete the
/// `replace_real_map` call and this fails, rather than
/// `StaticProperty.<clinit>` failing with `InternalError: null property:
/// java.home` in a `--jdk-only` run nothing in `cargo test` reaches.
///
/// Two halves, because either alone is a false green:
///
///  1. the two compatibility modes bind DIFFERENT bodies for the triple — a
///     runtime check, so a branch collapsed to one arm cannot pass it;
///  2. the `--jdk-only` body calls `replace_real_map` — a source check, because
///     nothing else here can execute a native without a live `NativeContext`.
#[test]
fn the_jdk_only_get_properties_arm_fills_the_real_map() {
    const TRIPLE: (&str, &str, &str) = (
        "java/lang/System",
        "getProperties",
        "()Ljava/util/Properties;",
    );
    const BODY: &str = "native_system_get_properties_jdk_only";
    const FILL: &str = "replace_real_map";

    let (class, method, descriptor) = TRIPLE;

    let production = production_registry();
    let mut jdk_only = NativeMethodRegistry::new();
    jdk_only.set_compatibility_mode(CompatibilityMode::JdkOnly);
    register_essential_natives(&mut jdk_only);
    assert!(
        jdk_only.compatibility_mode().is_jdk_only(),
        "the mode did not stick, so the branch under test was never taken"
    );

    let prod_body = production
        .find(class, method, descriptor)
        .expect("production mode must register System.getProperties");
    let jdk_body = jdk_only
        .find(class, method, descriptor)
        .expect("--jdk-only must register System.getProperties");
    assert!(
        !std::ptr::fn_addr_eq(prod_body, jdk_body),
        "both modes bound the same body for {class}.{method}{descriptor}; the \
         registration-time branch has collapsed, so --jdk-only is running the \
         --real-jdk body and the real Properties.map is null again"
    );

    // The source half. Located by the function's own name rather than by a
    // literal call expression: `cargo fmt` rewraps an argument list and a test
    // that reads the call TEXT goes red for a formatting change.
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs");
    let text = std::fs::read_to_string(&src).expect("lib.rs must be readable");
    let body = strip_test_modules(&text);
    let at = body
        .find(&format!("fn {BODY}("))
        .unwrap_or_else(|| panic!("{BODY} is gone from lib.rs; it is what fills Properties.map"));
    let end = body[at..]
        .find("\n}\n")
        .map(|rel| at + rel)
        .unwrap_or(body.len());
    assert!(
        body[at..end].contains(FILL),
        "{BODY} no longer calls {FILL}. The 185 triples in \
         RETIRED_SHADOW_PHASE3_TRIPLES read the real Properties.map, and \
         nothing else fills it."
    );
}
/// The `--jdk-only` `initPhase1` arm publishes `java.lang.System.props`.
///
/// Sibling of `the_jdk_only_get_properties_arm_fills_the_real_map`, one level
/// up the same cluster. `System.getProperty` IS `props.getProperty(key)` in the
/// real JDK, so a null static field is the first thing any real `System`
/// bytecode hits -- MEASURED as the first line of the first corpus vector under
/// `CRATONVM_ENFORCE_NATIVE_SHADOW=all`. The native `initPhase1` that replaced
/// the real one has always documented setting that field as step 1 of what it
/// models, and did not.
///
/// Both halves again, because either alone is a false green: the two modes must
/// bind DIFFERENT bodies (a collapsed branch cannot pass a runtime check), and
/// the `--jdk-only` body must reach the publisher (nothing here can execute a
/// native without a live `NativeContext`).
///
/// `--real-jdk` binding the UNCHANGED body is part of the contract, not an
/// omission: the singleton only acquires a real `map` on the `--jdk-only` path,
/// so publishing it in compatible mode would hand real bytecode a `Properties`
/// it still cannot read.
#[test]
fn the_jdk_only_init_phase1_arm_publishes_the_system_props_field() {
    const TRIPLE: (&str, &str, &str) = ("java/lang/System", "initPhase1", "()V");
    const BODY: &str = "native_system_init_phase1_jdk_only";
    const PUBLISH: &str = "publish_real_system_props";

    let (class, method, descriptor) = TRIPLE;

    let production = production_registry();
    let mut jdk_only = NativeMethodRegistry::new();
    jdk_only.set_compatibility_mode(CompatibilityMode::JdkOnly);
    register_essential_natives(&mut jdk_only);

    let prod_body = production
        .find(class, method, descriptor)
        .expect("production mode must register System.initPhase1");
    let jdk_body = jdk_only
        .find(class, method, descriptor)
        .expect("--jdk-only must register System.initPhase1");
    assert!(
        !std::ptr::fn_addr_eq(prod_body, jdk_body),
        "both modes bound the same body for {class}.{method}{descriptor}; the          registration-time branch has collapsed and System.props is null again"
    );

    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs");
    let text = std::fs::read_to_string(&src).expect("lib.rs must be readable");
    let body = strip_test_modules(&text);
    let at = body
        .find(&format!("fn {BODY}("))
        .unwrap_or_else(|| panic!("{BODY} is gone from lib.rs"));
    let end = body[at..]
        .find("
}
")
        .map(|rel| at + rel)
        .unwrap_or(body.len());
    assert!(
        body[at..end].contains(PUBLISH),
        "{BODY} no longer calls {PUBLISH}, so java.lang.System.props goes back          to null and real System.getProperty bytecode NPEs on its first call."
    );

    // The same arm publishes the OTHER static the real `initPhase1` sets, and
    // it is checked here rather than in a test of its own because they share
    // one body: a future edit that keeps the branch and drops one call would
    // pass every other assertion in this file.
    //
    // `SharedSecrets.javaLangAccess` is the larger of the two by blast radius.
    // Real JDK code captures it into statics of its own during class
    // initialisation (`sun.nio.cs.UTF_8.JLA`,
    // `jdk.internal.constant.ConstantUtils.JLA`), so a null propagates and then
    // persists -- MEASURED as the single largest family of first failures under
    // `CRATONVM_ENFORCE_NATIVE_SHADOW=all`, nine of sixteen sampled vectors.
    const PUBLISH_JLA: &str = "publish_shared_secrets";
    assert!(
        body[at..end].contains(PUBLISH_JLA),
        "{BODY} no longer calls {PUBLISH_JLA}. SharedSecrets.getJavaLangAccess()          is a shadow like any other; declined, the real accessor returns the          static, and nothing else sets it."
    );
}
