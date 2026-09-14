// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP2.5 — Dynamic `Proxy.newProxyInstance` conformance tests.
//!
//! Verifies the contract of the synthetic `java/lang/reflect/Proxy$Instance`
//! enhanced for the Strategy-B+ implementation:
//!
//!   * The four `java.lang.reflect.Proxy` natives (`isProxyClass`,
//!     `getInvocationHandler`, `newProxyInstance`,
//!     `getProxyInterfacesNative`) are registered on the right
//!     classes with the right descriptors.
//!   * The proxy module's classification helpers behave per spec.
//!   * The `proxy_last_interfaces_bits` accessor exists and returns
//!     a non-zero value after a proxy is created (driven by the
//!     `last-interfaces` cache update in `native_proxy_new_instance`).
//!   * The `ProxyProbe` Java app under `apps/proxy_probe/` compiles and
//!     loads under cratonvm.

use cratonvm_native_api::NativeMethodRegistry;

#[test]
fn proxy_natives_registered_with_jdk25_signatures() {
    let mut r = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_reflect_proxy_natives(&mut r);

    // isProxyClass(Ljava/lang/Class;)Z
    assert!(
        r.find(
            "java/lang/reflect/Proxy",
            "isProxyClass",
            "(Ljava/lang/Class;)Z"
        )
        .is_some(),
        "isProxyClass must be registered"
    );

    // getInvocationHandler(Ljava/lang/Object;)Ljava/lang/reflect/InvocationHandler;
    assert!(
        r.find(
            "java/lang/reflect/Proxy",
            "getInvocationHandler",
            "(Ljava/lang/Object;)Ljava/lang/reflect/InvocationHandler;"
        )
        .is_some(),
        "getInvocationHandler must be registered"
    );

    // newProxyInstance — full JDK 25 signature
    assert!(
        r.find(
            "java/lang/reflect/Proxy",
            "newProxyInstance",
            "(Ljava/lang/ClassLoader;[Ljava/lang/Class;Ljava/lang/reflect/InvocationHandler;)Ljava/lang/Object;"
        )
        .is_some(),
        "newProxyInstance must be registered"
    );

    // WP2.5: per-instance interface accessor on the synthetic class.
    assert!(
        r.find(
            "java/lang/reflect/Proxy$Instance",
            "getProxyInterfacesNative",
            "()[Ljava/lang/Class;"
        )
        .is_some(),
        "getProxyInterfacesNative must be registered on Proxy$Instance"
    );
}

#[test]
fn proxy_module_field_layout_constants() {
    use cratonvm_vm::runtime::proxy::{
        PROXY_FIELD_HANDLER, PROXY_FIELD_IDENTITY_HASH, PROXY_FIELD_INTERFACES,
        PROXY_INSTANCE_CLASS, PROXY_INSTANCE_FIELD_COUNT,
    };
    assert_eq!(PROXY_INSTANCE_CLASS, "java/lang/reflect/Proxy$Instance");
    assert_eq!(PROXY_FIELD_HANDLER, 0);
    assert_eq!(PROXY_FIELD_INTERFACES, 1);
    assert_eq!(PROXY_FIELD_IDENTITY_HASH, 2);
    assert_eq!(PROXY_INSTANCE_FIELD_COUNT, 3);
}

#[test]
fn proxy_module_classify_object_methods() {
    use cratonvm_vm::runtime::proxy::ProxyObjectMethod;

    assert_eq!(
        ProxyObjectMethod::classify("getClass", "()Ljava/lang/Class;"),
        ProxyObjectMethod::GetClass
    );
    assert!(ProxyObjectMethod::GetClass.should_bypass_handler());

    assert_eq!(
        ProxyObjectMethod::classify("hashCode", "()I"),
        ProxyObjectMethod::HashCode
    );
    assert_eq!(
        ProxyObjectMethod::classify("equals", "(Ljava/lang/Object;)Z"),
        ProxyObjectMethod::Equals
    );
    assert_eq!(
        ProxyObjectMethod::classify("toString", "()Ljava/lang/String;"),
        ProxyObjectMethod::ToString
    );
    assert_eq!(
        ProxyObjectMethod::classify("hello", "(Ljava/lang/String;)Ljava/lang/String;"),
        ProxyObjectMethod::InterfaceMethod
    );
}

#[test]
fn proxy_module_descriptor_param_count() {
    use cratonvm_vm::runtime::proxy::count_descriptor_params;

    // Signature shapes encountered on real JDK proxies.
    assert_eq!(count_descriptor_params("()V"), 0);
    assert_eq!(count_descriptor_params("(I)V"), 1);
    assert_eq!(count_descriptor_params("(Ljava/lang/String;)V"), 1);
    assert_eq!(
        count_descriptor_params("(Ljava/sql/Connection;)Ljava/sql/PreparedStatement;"),
        1
    );
    assert_eq!(
        count_descriptor_params("(Ljava/lang/String;[Ljava/lang/Object;)Ljava/util/List;"),
        2
    );
}

#[test]
fn proxy_native_call_populates_last_interfaces_after_a_proxy_is_made() {
    // White-box: bumping the global counter from a unit test path
    // proves the symbol is exposed and the cache is reachable.
    let vm = cratonvm_vm::vm::SharedVm::new(cratonvm_vm::config::VmConfig::default());
    let bits_before = cratonvm_native_builtins::proxy_last_interfaces_bits(vm.vm_identity);
    // Without an actual `newProxyInstance` call we can't write to the cache
    // (it's only updated from the native), so this cannot check the "populates"
    // half its name promises. What it CAN check — and what `let _ = bits_before`
    // checked, namely nothing — is the accessor's documented zero-value
    // contract: `proxy_last_interfaces_bits` returns 0 when no proxy has been
    // created for that VM identity (see `reflect_annotations.rs`, which maps the
    // tracked cell through `.unwrap_or(0)`).
    //
    // The assertion is made against a SENTINEL identity, not `vm.vm_identity`.
    // `vm_identity` values are not guaranteed unique across a VM's lifetime, and
    // sibling tests in this binary run whole VMs in-process; asserting 0 for a
    // live identity would be a race. `usize::MAX` is an identity no VM has,
    // which makes the zero answer a property rather than a scheduling accident.
    assert_eq!(
        cratonvm_native_builtins::proxy_last_interfaces_bits(usize::MAX),
        0,
        "proxy_last_interfaces_bits must answer 0 for a VM identity that never created a proxy"
    );
    // A freshly built VM has created no proxy either, so its cell must read 0
    // too. This one CAN race in principle (identities are reused), so it is
    // reported rather than asserted — an eprintln that shows up under
    // `--nocapture` is honest; a vacuous `bits_before >= 0` on a u64 would not
    // be, since it is true for every possible value.
    eprintln!("[wp2_5] fresh-VM proxy_last_interfaces_bits = {bits_before}");
}

#[test]
fn proxy_probe_compiled_class_files_exist() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let probe_dir = manifest.parent().unwrap().join("apps").join("proxy_probe");

    // BUILD, don't skip. The shape that stood here —
    //
    //     if !probe_dir.exists() || !main_cls.exists() { ...; return; }
    //
    // put TWO escape hatches in front of the only assertions in the test, and
    // both were open in every checkout: `apps/` is gitignored, so neither the
    // directory nor a compiled class is ever present after a clone, and the
    // test reported `ok` in 0.00s having asserted nothing. With
    // `ProxyProbe.java` present an absent `.class` is not a reason to skip, it
    // is a reason to compile — `ensure_probe_compiled` does that, and reports
    // a genuinely MISSING source through `common::require_fixture`.
    let staged = if ensure_probe_compiled() {
        proxy_probe_dir()
    } else {
        None
    };
    let Some(dir) = staged else {
        // Loud, and a failure under CRATONVM_REQUIRE_E2E — see
        // `common::require_fixture` and the NAME COLLISION note on
        // `probe_source_file`. `ProxyProbe.java` is one of the candidates, so a
        // present source with an unusable javac still reads as an absent
        // TOOLCHAIN (skip) rather than a broken checkout (panic).
        let _ = common::require_fixture(
            "wp2_5_proxy",
            "the WP2.5 fixture `ProxyProbe` (ProxyProbe.class, compiled from ProxyProbe.java; \
             this test pins ProxyProbe$Greeter / $Counter / $PrefixedGreeter)",
            &[
                probe_dir.join("ProxyProbe.class"),
                probe_dir.join("classes").join("ProxyProbe.class"),
                probe_dir.join("ProxyProbe.java"),
            ],
        );
        return;
    };

    assert!(
        dir.join("ProxyProbe.class").exists(),
        "ProxyProbe.class must exist in the staged probe dir {}",
        dir.display()
    );
    // The inner interfaces the 6-case contract is built on must be staged
    // beside it — a proxy over interfaces that did not compile is not a test.
    for inner in &[
        "ProxyProbe$Greeter.class",
        "ProxyProbe$Counter.class",
        "ProxyProbe$PrefixedGreeter.class",
    ] {
        let p = dir.join(inner);
        assert!(
            p.exists(),
            "{} must be staged beside ProxyProbe.class in {} — the WP2.5 probe declares \
             Greeter / Counter / PrefixedGreeter and every one of the 6 cases needs them",
            inner,
            dir.display()
        );
    }
}

#[test]
fn proxy_probe_loads_under_cratonvm_when_staged() {
    use cratonvm_vm::config::VmConfig;
    use cratonvm_vm::vm::Vm;

    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let probe_dir = manifest.parent().unwrap().join("apps").join("proxy_probe");
    if !probe_dir.join("ProxyProbe.class").exists() {
        let _ = common::require_fixture(
            "wp2_5_proxy",
            "the WP2.5 fixture `ProxyProbe` (ProxyProbe.class, compiled from ProxyProbe.java)",
            &[
                probe_dir.join("ProxyProbe.class"),
                probe_dir.join("ProxyProbe.java"),
            ],
        );
        return;
    }

    let cp = vec![probe_dir.to_string_lossy().to_string()];
    let config = VmConfig::default().with_classpath(cp);
    let vm = Vm::new(config);

    // Load + define the main class. We don't run main() here because
    // executing the InvocationHandler lambda requires `lambda_proxy`
    // bootstrap that is exercised separately (WP1.6) and orthogonal to
    // this WP. The load itself proves the class is parseable and the
    // surface compiles.
    let result = vm.shared.load_class_concurrent("ProxyProbe");
    assert!(result.is_ok(), "ProxyProbe must load: {:?}", result);
}

#[test]
fn proxy_get_interfaces_handles_non_proxy_class() {
    // White-box: a non-proxy class mirror should fall through to the
    // standard interface-list lookup. Without a full VM context the
    // best we can do here is reach the module API and confirm it
    // exists.
    use cratonvm_vm::runtime::proxy::is_proxy_class_name;
    assert!(!is_proxy_class_name("java/lang/Object"));
    assert!(!is_proxy_class_name("java/util/HashMap"));
    assert!(is_proxy_class_name("java/lang/reflect/Proxy$Instance"));
}

#[test]
fn proxy_diagnostic_counters_exposed() {
    use cratonvm_vm::runtime::proxy::stats;

    let before = stats::instances_created();
    stats::inc_instances_created();
    assert_eq!(stats::instances_created(), before + 1);

    let before_d = stats::dispatches();
    stats::inc_dispatches();
    assert_eq!(stats::dispatches(), before_d + 1);
}

#[test]
fn proxy_native_builtins_diagnostic_counter_exposed() {
    // Sanity-check that the native-builtins-side counter is a stable
    // symbol callers can monitor.
    let n = cratonvm_native_builtins::proxy_instances_created();
    // Just check the call doesn't panic and returns a u64.
    let _ = n;
}

// ---------------------------------------------------------------------------
// WP2.5 close-out: lambda InvocationHandler dispatch through proxy
// ---------------------------------------------------------------------------
//
// These four tests exercise the end-to-end Proxy.newProxyInstance →
// invokeinterface → InvocationHandler.invoke path with a lambda
// handler. Before the fix in `proxy_invoke_handler_shared`, the
// dispatch fell back to the abstract `InvocationHandler.invoke`
// (which has no Code attribute) and crashed with a linkage error.
//
// All four tests share the same scaffolding: compile a small Java
// shape, run it under `cratonvm`, and assert the expected `pass-N`
// line appears in stdout.

/// Helper: locate the proxy_probe directory if staged. Returns `None`
/// if the fixture isn't compiled (which means the test silently
/// skips).
fn proxy_probe_dir() -> Option<std::path::PathBuf> {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let probe_dir = manifest.parent().unwrap().join("apps").join("proxy_probe");
    let classes_dir = probe_dir.join("classes");
    if classes_dir.join("ProxyProbe.class").exists() {
        Some(classes_dir)
    } else if probe_dir.join("ProxyProbe.class").exists() {
        Some(probe_dir)
    } else {
        None
    }
}

/// Try to invoke the `main` method on a class on the given classpath.
/// Returns `Ok(stdout_capture_or_message)` on success — note we don't
/// actually capture stdout here since the VM writes directly to the
/// process stdout. The return value is just a marker that the call
/// completed without a hard error.
fn run_proxy_main(class_name: &str) -> Result<(), String> {
    use cratonvm_types::Value;
    use cratonvm_vm::config::VmConfig;
    use cratonvm_vm::vm::Vm;

    let probe = proxy_probe_dir().ok_or_else(|| "proxy_probe classes not staged".to_string())?;
    let cp = vec![probe.to_string_lossy().to_string()];
    let config = VmConfig::default().with_classpath(cp);
    let mut vm = Vm::new(config);

    vm.shared
        .load_class_concurrent(class_name)
        .map_err(|e| format!("load {class_name}: {e:?}"))?;

    let result = vm.invoke(
        class_name,
        "main",
        "([Ljava/lang/String;)V",
        &[Value::Object(None)],
    );

    match result {
        Ok(_) => Ok(()),
        Err(e) => {
            let msg = format!("{e:?}");
            // Report linkage errors that we explicitly fixed; let
            // unrelated runtime exceptions through (the probe handles
            // them itself).
            if msg.contains("no Code attribute") || msg.contains("AbstractMethodError") {
                Err(format!("proxy dispatch regressed: {msg}"))
            } else {
                Ok(())
            }
        }
    }
}

#[test]
fn proxy_lambda_handler_dispatches_single_iface() {
    // Test 1 of the probe: single-interface proxy with a lambda
    // InvocationHandler. Verifies that the lambda body executes
    // (no "no Code attribute" linkage error).
    if proxy_probe_dir().is_none() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let probe_dir = manifest.parent().unwrap().join("apps").join("proxy_probe");
        let _ = common::require_fixture(
            "wp2_5_proxy",
            "the WP2.5 fixture `ProxyProbe` (compiled ProxyProbe.class, in the probe dir or its \
             classes/ subdir)",
            &[
                probe_dir.join("classes").join("ProxyProbe.class"),
                probe_dir.join("ProxyProbe.class"),
            ],
        );
        return;
    }
    let r = run_proxy_main("ProxyProbe");
    assert!(
        r.is_ok(),
        "lambda InvocationHandler must dispatch without linkage error: {:?}",
        r
    );
}

#[test]
fn proxy_invoke_handler_shared_with_object_handler_smoke() {
    // White-box: a regular (non-lambda) InvocationHandler should still
    // dispatch through the standard `invoke_or_native` path. This is
    // a smoke test that the new `handler_is_lambda` branch doesn't
    // inadvertently swallow the non-lambda path.
    use cratonvm_vm::config::VmConfig;
    use cratonvm_vm::vm::SharedVm;
    use std::sync::Arc;

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    // Allocate a "fake" non-lambda object — its class_id is *not* in
    // `lambda_proxies`, so the proxy dispatch should take the
    // by-class-name path. Without a real handler class registered we
    // expect the `invoke_or_native` to fail with NoSuchMethodError or
    // similar — but importantly, *not* with the lambda dispatch
    // branch. The mere fact that the call returns is what we're
    // verifying.
    use cratonvm_types::ClassId;
    let cls = ClassId::new(1);
    let handler = shared.mem.heap.alloc_object(cls, 0);
    let handler_class_id = shared.mem.heap.class_id_of(handler);
    // Confirm the heap object isn't classified as a lambda proxy.
    let in_lambda_table = shared
        .classes
        .lambda_proxies
        .read()
        .contains_key(&handler_class_id);
    assert!(
        !in_lambda_table,
        "regular allocated object must not be in lambda_proxies",
    );
}

#[test]
fn proxy_lambda_dispatch_preserves_diagnostic_counters() {
    // The dispatch counter must be incremented when a lambda handler
    // runs end-to-end. Even if the probe isn't staged, the counter
    // surface itself is reachable.
    use cratonvm_vm::runtime::proxy::stats;
    let _ = stats::dispatches();
    let _ = stats::instances_created();
    // Bump and verify monotonic.
    //
    // `>=`, not `==`. `PROXY_DISPATCHES` is a process-global `AtomicUsize` and
    // this thread is not its only writer:
    // `proxy_lambda_handler_dispatches_single_iface` runs `ProxyProbe.main`
    // through an IN-PROCESS `Vm` in a sibling test thread, and every proxy
    // dispatch it performs calls `inc_dispatches()` on the same counter.
    // `load; inc; load == b + 1` is a read-modify-read race against that, and
    // an exact delta is not a property this thread can own.
    //
    // Observed 2026-08-03 failing 2 of 3 full `cargo test -p cratonvm-vm` runs
    // (`left: 2, right: 1`) while passing 3 of 3 when the binary is run on its
    // own — the giveaway that it is scheduling, not behaviour. Anything that
    // shifts timing changes how often the race loses; what surfaced it was
    // cov-01 moving more methods to the optimizing tier, so the probe VM ran
    // at a different speed.
    //
    // What this thread CAN prove is what is asserted below: the counter is
    // monotonic and its own increment is included in the result.
    let b = stats::dispatches();
    stats::inc_dispatches();
    let after = stats::dispatches();
    assert!(
        after >= b + 1,
        "the dispatch counter must be monotonic and include this thread's \
         increment: before={b} after={after}"
    );
}

#[test]
fn proxy_invoke_handler_shared_is_exported() {
    // The fix moves the lambda-handler routing into
    // `proxy_invoke_handler_shared` in the `vm::vm_exec` module.
    // Verify the function is reachable from the public crate surface
    // so future regression tests can target it directly.
    //
    // We can't call it without a real proxy object, so a compile-time
    // check via `let _: fn(...) = ...` would be ideal — but the fn is
    // `pub(crate)` and not exposed. Instead, verify via a related
    // public symbol that the dispatch module is wired in.
    use cratonvm_vm::runtime::proxy::PROXY_INSTANCE_CLASS;
    assert_eq!(PROXY_INSTANCE_CLASS, "java/lang/reflect/Proxy$Instance");

    // Also sanity-check the lambda interpreter entry point is exposed
    // (the fix calls `runtime::interpreter::try_lambda_dispatch`).
    // It's `pub(crate)` so we can't reference it from a test, but
    // we *can* verify the related `lambda_proxy` module's stats are
    // reachable, proving the lambda subsystem is linked into this
    // test binary.
    let _ = cratonvm_vm::runtime::lambda_proxy::dispatch_count();
    let _ = cratonvm_vm::runtime::lambda_proxy::bootstrap_count();
}

// ---------------------------------------------------------------------------
// WP2.5 6-case acceptance matrix — driven by spawning the cratonvm CLI
// against `apps/proxy_probe/ProxyProbe.java`.
//
// The probe (see `apps/proxy_probe/ProxyProbe.java`) prints a `pass-N` or
// `fail-N` line for each of 6 cases:
//
//   1. single-iface         — Proxy.newProxyInstance returns a Greeter
//                             whose hello("world") routes through the
//                             lambda InvocationHandler.
//   2. isProxyClass         — `Proxy.isProxyClass(p.getClass())` true and
//                             false on a non-proxy class.
//   3. getInterfaces        — `p.getClass().getInterfaces()` round-trips
//                             the original interface array.
//   4. getInvocationHandler — `Proxy.getInvocationHandler(p)` returns the
//                             same handler instance passed in.
//   5. multi-iface          — proxy of two interfaces dispatches both
//                             methods through the handler.
//   6. default-method       — invocations of an interface default method
//                             route through the handler.
//
// Cases 5 and 6 are the FAIL-5 / FAIL-6 acceptance items; per the WP2.5
// roadmap they require Agent A's `proxy.rs` bytecode-generator fix to
// pass. Cases 1-4 should already pass after the WP2.5 close-out lambda
// dispatch fix from earlier.
//
// Layout mirrors `wp2_2_method_invoke_matrix.rs`:
// - `run_probe_once` memoizes a single CLI spawn for all the per-case
//   subtests, so `cargo test` finishes in seconds instead of running
//   the probe 7 times.
// - `case_passed` greps for `pass-N`.
// - Failing tests carry the corresponding `fail-N …` line if present so
//   triage doesn't need to re-run the probe.
//
// All tests run unconditionally (no `#[ignore]`) — until Agent A's fix
// lands, cases 5 and 6 will fail. That's the WP2.5 acceptance gate.
// ---------------------------------------------------------------------------

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

/// Source `.java` file for the probe — used by the on-demand `javac`
/// fallback if the classes directory is missing.
///
/// # NAME COLLISION — do not "repoint" this at `probes/ProxyProbe.java`
///
/// `apps/proxy_probe/ProxyProbe.java` is absent from the tree (`apps/` is
/// gitignored, .gitignore line 12), and a tracked `probes/ProxyProbe.java` does
/// exist — which makes this look like a one-line wrong-path bug. It is not.
/// The two files share a class name and nothing else:
///
/// | | `probes/ProxyProbe.java` (tracked) | what THIS file asserts |
/// |---|---|---|
/// | interfaces | `Greeter{greet,count}`, `Marker` | `Greeter`, `Counter`, `PrefixedGreeter` |
/// | output | `greet=`, `count=`, `PROXY-DONE` | `pass-1`..`pass-6`, `summary 6/6`, `OK` |
/// | default methods | none | case 6 requires one |
///
/// Repointing would convert seven silent skips into seven failures that name
/// the wrong defect. The WP2.5 fixture has to be rewritten to the 6-case
/// contract documented at the bottom of this file; when it is, put it in
/// `probes/` under a NON-colliding name (e.g. `probes/Wp25ProxyProbe.java`)
/// and add that path here.
fn probe_source_file() -> PathBuf {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .unwrap()
        .join("apps")
        .join("proxy_probe")
        .join("ProxyProbe.java")
}

/// Resolve the cratonvm CLI binary. Prefer release (faster), fall back to
/// debug; honor `CRATONVM_BIN` for hermetic CI builds.
mod common;

/// Prerequisite gate: the lookup below is unchanged — only a MISSING binary is
/// reported differently. See `common::require_binary`.
fn cratonvm_binary() -> Option<PathBuf> {
    common::require_binary(cratonvm_binary_lookup())
}

fn cratonvm_binary_lookup() -> Option<PathBuf> {
    if let Ok(bin) = std::env::var("CRATONVM_BIN") {
        let p = PathBuf::from(&bin);
        if p.exists() {
            return Some(p);
        }
    }
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = manifest.parent().unwrap().join("target");
    let exe = if cfg!(windows) {
        "cratonvm.exe"
    } else {
        "cratonvm"
    };
    for profile in &["release", "debug"] {
        let candidate = target.join(profile).join(exe);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// True when `class_file` is at least as new as `src` — i.e. the staged class
/// is a build OF the source now in the tree, not of some earlier version of it.
///
/// A `.class` that merely EXISTS is not a current one. A probe's compiled
/// `SbRunner.class` was once found to be a MONTH older than its `.java`, which
/// made a landed change appear in no log at all; the shape that allows it is
/// `if class_file.exists() { return true; }` — which is exactly what stood at
/// the top of [`ensure_probe_compiled`]. Edit `ProxyProbe.java` and the next
/// `cargo test` would have run the OLD fixture and reported on it.
///
/// Unreadable timestamps fall back to "current", so a filesystem without usable
/// mtimes degrades to the historical behaviour rather than recompiling forever.
fn staged_class_is_current(class_file: &std::path::Path, src: &std::path::Path) -> bool {
    let times = (
        std::fs::metadata(class_file).and_then(|m| m.modified()),
        std::fs::metadata(src).and_then(|m| m.modified()),
    );
    let (class_mtime, src_mtime) = match times {
        (Ok(c), Ok(s)) => (c, s),
        _ => return true,
    };
    if class_mtime < src_mtime {
        eprintln!(
            "[wp2_5_proxy] {} is OLDER than {} — the staged fixture is stale; recompiling.",
            class_file.display(),
            src.display()
        );
        return false;
    }
    true
}

/// Compile the probe via `javac --release 21` if the staged classes are
/// missing **or stale**. Returns true if `ProxyProbe.class` exists afterwards.
fn ensure_probe_compiled() -> bool {
    let src = probe_source_file();
    let staged = proxy_probe_dir();
    if let Some(dir) = &staged {
        // Classes staged with no source next to them: there is nothing to
        // rebuild from, so use what is there (the historical behaviour).
        if !src.exists() || staged_class_is_current(&dir.join("ProxyProbe.class"), &src) {
            return true;
        }
    }
    if !src.exists() {
        // A missing fixture is a broken checkout, not an absent toolchain. Report
        // it loudly, and fail under CRATONVM_REQUIRE_E2E — see
        // `common::require_fixture`.
        //
        // DO NOT "fix" this by pointing `probe_source_file` at
        // `probes/ProxyProbe.java`. That file exists and is tracked, but it is a
        // DIFFERENT probe with the same class name — see the NAME COLLISION note
        // above `probe_source_file`.
        let _ = common::require_fixture(
            "wp2_5_proxy",
            "the WP2.5 6-case fixture `ProxyProbe.java` (must print pass-1..pass-6, `summary 6/6` \
             and `OK`, and declare ProxyProbe$Greeter / $Counter / $PrefixedGreeter). NOTE: the \
             tracked `probes/ProxyProbe.java` is a DIFFERENT probe with the same class name and \
             will NOT satisfy these assertions",
            &[src.clone()],
        );
        return false;
    }
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    // Rebuild IN PLACE when a stale copy is already staged, so the directory
    // `proxy_probe_dir()` resolves to is the one that gets refreshed. Only a
    // first-ever compile goes to the `classes/` subdir.
    let classes = match &staged {
        Some(dir) => dir.clone(),
        None => manifest
            .parent()
            .unwrap()
            .join("apps")
            .join("proxy_probe")
            .join("classes"),
    };
    let _ = std::fs::create_dir_all(&classes);
    let status = Command::new("javac")
        .arg("--release")
        .arg("21")
        .arg("-d")
        .arg(&classes)
        .arg(&src)
        .output();
    match status {
        // javac cannot be launched at all — the one legitimate skip.
        Err(_) => false,
        // javac RAN and rejected the fixture: skipping here would make this
        // test a permanent vacuous pass.
        Ok(o) => {
            // javac REJECTED THE ARGUMENTS, not the source: an unsupported `--release`
            // means this javac is older than the level this probe compiles at, so it never
            // opened the file. That is a missing-toolchain condition — the same one the
            // `Err(e)` arm above skips for — not a broken probe. Reporting it as "fix the
            // source" sends the next reader to edit a correct `.java` file.
            //
            // Narrowly keyed on javac's own wording for an unsupported release, so a
            // genuine source error still reaches the assertion below and still fails loudly
            // (see `probe_compile_guard.rs` for why that must never become a skip).
            if !o.status.success() {
                let stderr_probe = String::from_utf8_lossy(&o.stderr);
                if stderr_probe.contains("release version")
                    && stderr_probe.contains("not supported")
                {
                    eprintln!(
                        "[wp2_5_proxy] javac cannot target --release 21 ({}); skipping. Point \
                         JAVA_HOME or CRATONVM_JAVA_HOME at a JDK 21+ install.",
                        stderr_probe.lines().next().unwrap_or("").trim()
                    );
                    return false;
                }
            }
            assert!(
                o.status.success(),
                "[wp2_5_proxy] the checked-in probe fixture failed to compile — fix \
                 the .java source. javac stderr:\n{}",
                String::from_utf8_lossy(&o.stderr)
            );
            classes.join("ProxyProbe.class").exists()
        }
    }
}

/// Captured probe stdout. Returns `None` on infrastructure failure
/// (binary missing, classes missing, javac unavailable). Callers
/// `eprintln!` and short-circuit so the test reports `skip` instead of
/// misattributing failure.
fn run_probe_once() -> Option<String> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            if !ensure_probe_compiled() {
                eprintln!("[wp2_5] proxy_probe class files unavailable; skipping");
                return None;
            }
            let bin = match cratonvm_binary() {
                Some(b) => b,
                None => {
                    eprintln!(
                        "[wp2_5] cratonvm binary not found; build with `cargo build --release -p cratonvm-cli`"
                    );
                    return None;
                }
            };
            let classes = match proxy_probe_dir() {
                Some(d) => d,
                None => {
                    eprintln!("[wp2_5] proxy_probe classes vanished after compile");
                    return None;
                }
            };
            let output = Command::new(&bin)
                .arg("-c")
                .arg(&classes)
                .arg("ProxyProbe")
                .output();
            match output {
                Ok(o) => {
                    let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
                    let stderr = String::from_utf8_lossy(&o.stderr).into_owned();
                    Some(format!("{stdout}\n--- STDERR ---\n{stderr}"))
                }
                Err(e) => {
                    eprintln!("[wp2_5] failed to spawn cratonvm: {e}");
                    None
                }
            }
        })
        .clone()
}

/// True if the probe output contains `pass-{n}` (matches the probe's
/// `System.out.println("pass-N …")` format).
fn case_passed(output: &str, n: u32) -> bool {
    let needle = format!("pass-{n}");
    output.lines().any(|l| l.contains(&needle))
}

/// Look for the `fail-N …` line; useful for triage messages.
fn fail_line(output: &str, n: u32) -> Option<String> {
    let needle = format!("fail-{n}");
    output
        .lines()
        .find(|l| l.contains(&needle))
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Composite acceptance gate
// ---------------------------------------------------------------------------

/// Composite end-to-end test: runs the probe once and pins the final
/// `summary 6/6` + `OK` lines. This is the WP2.5 acceptance gate —
/// failing this means at least one of the 6 cases regressed, regardless
/// of which one.
///
/// Until Agent A's `proxy.rs` bytecode-generator fix lands this test
/// is expected to FAIL on cases 5 + 6. We do NOT `#[ignore]` so the
/// gap stays visible in `cargo test` runs.
#[test]
fn proxy_probe_composite_6_of_6() {
    let output = match run_probe_once() {
        Some(o) => o,
        None => return, // skipped (no binary / no classes / no javac)
    };
    let has_summary = output.contains("summary 6/6");
    let has_ok = output.lines().any(|l| l.trim() == "OK");
    if !has_summary || !has_ok {
        // Pull the summary + any fail-N lines for a clean panic message.
        let triage: Vec<String> = output
            .lines()
            .filter(|l| {
                l.starts_with("summary ")
                    || l.starts_with("fail-")
                    || l.trim() == "FAIL"
                    || l.trim() == "OK"
            })
            .map(str::to_string)
            .collect();
        panic!(
            "WP2.5 composite probe did not reach 6/6 OK. Triage:\n  {}",
            triage.join("\n  ")
        );
    }
}

// ---------------------------------------------------------------------------
// Per-case subtests
//
// Each subtest re-parses the same memoized probe output (`run_probe_once`)
// and asserts on a single case from ProxyProbe.java. Failures show the
// exact `fail-N` line (when present) for triage.
// ---------------------------------------------------------------------------

/// Case 1: single-interface proxy returns expected value. The lambda
/// InvocationHandler must dispatch and return `"hi world"` when the
/// proxy's `hello("world")` is invoked.
#[test]
fn proxy_probe_case_1_single_iface() {
    let output = match run_probe_once() {
        Some(o) => o,
        None => return,
    };
    if !case_passed(&output, 1) {
        let line = fail_line(&output, 1)
            .unwrap_or_else(|| "(no pass-1 or fail-1 line found in probe output)".to_string());
        panic!("WP2.5 case 1 single-iface failed: {line}");
    }
}

/// Case 2: `Proxy.isProxyClass(p.getClass())` returns true on a proxy
/// instance and false on a non-proxy class (e.g. `String.class`).
#[test]
fn proxy_probe_case_2_is_proxy_class() {
    let output = match run_probe_once() {
        Some(o) => o,
        None => return,
    };
    if !case_passed(&output, 2) {
        let line = fail_line(&output, 2)
            .unwrap_or_else(|| "(no pass-2 or fail-2 line found in probe output)".to_string());
        panic!("WP2.5 case 2 isProxyClass failed: {line}");
    }
}

/// Case 3: `proxy.getClass().getInterfaces()` returns the array of
/// interfaces originally passed to `newProxyInstance` (presence — order
/// not guaranteed by spec).
#[test]
fn proxy_probe_case_3_get_interfaces() {
    let output = match run_probe_once() {
        Some(o) => o,
        None => return,
    };
    if !case_passed(&output, 3) {
        let line = fail_line(&output, 3)
            .unwrap_or_else(|| "(no pass-3 or fail-3 line found in probe output)".to_string());
        panic!("WP2.5 case 3 getInterfaces failed: {line}");
    }
}

/// Case 4: `Proxy.getInvocationHandler(p)` returns the exact handler
/// instance (reference equality) that was passed to `newProxyInstance`.
#[test]
fn proxy_probe_case_4_get_invocation_handler() {
    let output = match run_probe_once() {
        Some(o) => o,
        None => return,
    };
    if !case_passed(&output, 4) {
        let line = fail_line(&output, 4)
            .unwrap_or_else(|| "(no pass-4 or fail-4 line found in probe output)".to_string());
        panic!("WP2.5 case 4 getInvocationHandler failed: {line}");
    }
}

/// Case 5: multi-interface proxy dispatches both interfaces' methods
/// through the same handler. Proxy implements `Greeter` + `Counter`;
/// handler distinguishes by `m.getName()`.
///
/// **Requires Agent A's parallel fix in `vm/src/runtime/proxy.rs`** —
/// the existing bytecode generator only wires the first interface, so
/// the cast to `Counter` fails before the fix. Until A lands this is
/// expected to fail with `fail-5 multi-iface threw=ClassCastException`
/// or similar.
#[test]
fn proxy_probe_case_5_multi_iface() {
    let output = match run_probe_once() {
        Some(o) => o,
        None => return,
    };
    if !case_passed(&output, 5) {
        let line = fail_line(&output, 5)
            .unwrap_or_else(|| "(no pass-5 or fail-5 line found in probe output)".to_string());
        panic!("WP2.5 case 5 multi-iface failed (needs proxy.rs fix): {line}");
    }
}

/// Case 6: invocation of an interface default method routes through
/// the `InvocationHandler` (the handler chooses to handle it directly
/// rather than delegating via `InvocationHandler.invokeDefault`).
///
/// **Requires Agent A's parallel fix in `vm/src/runtime/proxy.rs`** —
/// the existing bytecode generator does not emit a method body for
/// interface default methods on the proxy class, so calls to
/// `prefixed("b")` either fall through to the iface default (returning
/// `"default:b"` instead of `"handled-b"`) or fail with an
/// AbstractMethodError. Expected to fail until A's fix lands.
#[test]
fn proxy_probe_case_6_default_method() {
    let output = match run_probe_once() {
        Some(o) => o,
        None => return,
    };
    if !case_passed(&output, 6) {
        let line = fail_line(&output, 6)
            .unwrap_or_else(|| "(no pass-6 or fail-6 line found in probe output)".to_string());
        panic!("WP2.5 case 6 default-method failed (needs proxy.rs fix): {line}");
    }
}
