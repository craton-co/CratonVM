// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! PGO-02 (docs/feature-designs/profile-guided-inlining.md, retired from
//! docs/known-issues/c2/archive/pgo-02-guarded-inlining.md): guarded monomorphic
//! virtual/interface inlining, end to end through a real single-pass JIT
//! compile with `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE` on.
//!
//! Every call site is driven by REPEATED INVOCATIONS from this test (not an
//! internal Java loop) so the method's own invocation-count tiering compiles
//! it — the code path this lane touches. An internal loop would instead
//! need OSR (a different, untouched compile path) to compile mid-call.
//!
//! `CRATONVM_JIT_GUARDED_VIRTUAL_INLINE` and `is_profiling_enabled` are
//! process-global, so the whole file runs under one lock and one
//! `#[test]` fn to avoid cross-test races (same reasoning as
//! `pgo01_call_site_evidence.rs`).

use cratonvm_types::flags::with_process_overrides;
use cratonvm_vm::config::VmConfig;
use cratonvm_vm::jit::profile::enable_profiling;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

fn test_resources_dir() -> String {
    if let Some(generated) = option_env!("CRATONVM_TEST_CLASSES_DIR") {
        if std::path::Path::new(generated).is_dir() {
            return generated.to_owned();
        }
    }
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    format!("{manifest_dir}/tests/resources")
}

fn class_files_available() -> bool {
    let dir = test_resources_dir();
    let class_path = format!("{dir}/cratonvm/PgoGuardedVirtualInline.class");
    std::path::Path::new(&class_path).exists()
}

fn test_vm() -> Vm {
    let config = VmConfig::new().with_classpath(vec![test_resources_dir()]);
    Vm::new(config)
}

fn invoke_int(vm: &mut Vm, method: &str, args: &[Value]) -> i32 {
    match vm.invoke("cratonvm/PgoGuardedVirtualInline", method, method_descriptor(method), args) {
        Ok(Some(Value::Int(n))) => n,
        other => panic!("{method} failed or returned a non-int: {other:?}"),
    }
}

fn invoke_void(vm: &mut Vm, method: &str, args: &[Value]) {
    match vm.invoke("cratonvm/PgoGuardedVirtualInline", method, method_descriptor(method), args) {
        Ok(None) => {}
        other => panic!("{method} failed or returned a value: {other:?}"),
    }
}

fn method_descriptor(method: &str) -> &'static str {
    match method {
        "callA" | "callCurrent" | "callThrowerCaught" | "callOverride" | "callIface" => "(I)I",
        "callPoly" => "(II)I",
        "setCurrent" => "(I)V",
        other => panic!("unknown method: {other}"),
    }
}

/// Past the default `CRATONVM_JIT_THRESHOLD` (500) so every entry point
/// below is guaranteed to have compiled by the time its loop finishes —
/// verified empirically (a lower value plus a `CRATONVM_JIT_THRESHOLD`
/// override left `callA` uncompiled; this test cares about a REAL compile
/// happening, not just a low iteration count, so it uses the real default
/// instead of chasing why the override didn't take).
const CALLS: i32 = 700;

#[test]
fn test_pgo02_guarded_virtual_inline() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    if !class_files_available() {
        eprintln!(
            "Skipping test_pgo02_guarded_virtual_inline: .class files not available (javac not on PATH?)"
        );
        return;
    }

    enable_profiling(true);
    let result = with_process_overrides(
        &[
            ("CRATONVM_JIT_GUARDED_VIRTUAL_INLINE", Some("1")),
            // Pin these methods to the SINGLE-PASS backend, which is the only
            // one that has the capability under test: guarded monomorphic
            // inlining is planned by `x64`'s inliner and reported through
            // `CompiledMethod::inline_tally`, and the optimizing (IR) backend
            // serves a virtual site from a MIC/PIC cascade instead and records
            // no tally at all.
            //
            // This used to happen by accident. Every entry point here is
            // `getstatic <A>; invokevirtual tag`, and until cov-01 the IR
            // builder had no `0xb2` arm — so the optimizing tier refused these
            // methods and the C1 artifact was what stayed in the cache. Once
            // `getstatic` lowered, `callA` was compiled by C2 on the next
            // promotion, its `inline_tally` was empty, and this test failed
            // with "speculative_sites == 0". The dependency was real; it was
            // just never written down.
            //
            // `CRATONVM_JIT_IR_CALL_VIRTUAL=0` is the declared opt-out for
            // `invokevirtual`/`invokeinterface` on the IR path. With it off the
            // builder has no `invoke_info` for those sites and bails the whole
            // method, which is exactly the routing this file needs.
            ("CRATONVM_JIT_IR_CALL_VIRTUAL", Some("0")),
        ],
        run_all_checks,
    );
    enable_profiling(false);
    if let Err(msg) = result {
        panic!("{msg}");
    }
}

fn run_all_checks() -> Result<(), String> {
    check_guard_hit()?;
    check_guard_miss()?;
    check_override_receiver()?;
    check_interface_site()?;
    check_thrower()?;
    check_polymorphic()?;
    Ok(())
}

/// The constant-pool class and the speculated receiver class disagree.
///
/// `callOverride`'s site is `invokevirtual A.tag` (javac uses the receiver
/// expression's STATIC type) but every receiver is exactly `B`, which
/// overrides `tag`. The guard is emitted against B's class id, so the body
/// behind it must be B's. Resolving the callee from the constant-pool class
/// name instead splices A's body behind a B guard, and every call quietly
/// returns `x+1` instead of `x+1000`.
fn check_override_receiver() -> Result<(), String> {
    let mut vm = test_vm();
    for i in 0..CALLS {
        let got = invoke_int(&mut vm, "callOverride", &[Value::Int(i)]);
        let want = i + 1000;
        if got != want {
            return Err(format!(
                "check_override_receiver: callOverride({i}) = {got}, want {want} (call #{i} of \
                 {CALLS}) — the guard admitted a receiver of class B and ran a body that is not \
                 B's `tag`"
            ));
        }
    }
    Ok(())
}

/// `invokeinterface` with a single implementation. Resolution that starts at
/// the constant-pool class finds `Tagger.itag`'s ABSTRACT declaration, which
/// has no `Code` attribute, so the site can never be spliced; resolution from
/// the speculated receiver class finds `OnlyImpl.itag` and can. Asserts both
/// the result and that a speculative site was actually admitted, so a
/// regression back to "correct but never inlined" is visible.
fn check_interface_site() -> Result<(), String> {
    let mut vm = test_vm();
    for i in 0..CALLS {
        let got = invoke_int(&mut vm, "callIface", &[Value::Int(i)]);
        let want = i + 77;
        if got != want {
            return Err(format!(
                "check_interface_site: callIface({i}) = {got}, want {want}"
            ));
        }
    }
    let tally = compiled_tally(&vm, "callIface")?;
    if tally.speculative_sites == 0 {
        return Err(format!(
            "check_interface_site: callIface compiled but speculative_sites == 0 (tally={tally:?}) \
             — an interface site with one implementation must be reachable by the guarded \
             inliner"
        ));
    }
    Ok(())
}

/// The `inline_tally` of a compiled entry point, with the tier dependency
/// stated rather than relied on (an IR artifact leaves the tally zeroed, which
/// is the opposite conclusion from "the guard did not fire").
fn compiled_tally(vm: &Vm, method: &str) -> Result<cratonvm_jit::InlineDecisionTally, String> {
    let class_id = vm
        .shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id("cratonvm/PgoGuardedVirtualInline")
        .ok_or_else(|| "class must be loaded after invoke".to_string())?;
    let compiled = vm
        .shared
        .jit
        .jit_cache
        .read()
        .get(
            "cratonvm/PgoGuardedVirtualInline",
            method,
            method_descriptor(method),
            class_id,
        )
        .ok_or_else(|| format!("{method} never JIT-compiled"))?;
    if compiled.used_ir_backend {
        return Err(format!(
            "{method} was compiled by the OPTIMIZING (IR) backend, which plans no guarded \
             inlines and records no inline_tally — the tier pin did not take"
        ));
    }
    Ok(compiled.inline_tally.clone())
}

/// Positive control: a fixed monomorphic-A call site, called past the
/// (lowered) JIT threshold, must keep returning the correct result once
/// compiled — the guard-hit path must be observably identical to the
/// interpreted / unguarded-dispatch result.
fn check_guard_hit() -> Result<(), String> {
    let mut vm = test_vm();
    for i in 0..CALLS {
        let got = invoke_int(&mut vm, "callA", &[Value::Int(i)]);
        let want = i + 1;
        if got != want {
            return Err(format!(
                "check_guard_hit: callA({i}) = {got}, want {want} (call #{i} of {CALLS})"
            ));
        }
    }
    // Positive proof the NEW code path actually ran, not just that results
    // happened to be correct (which normal dispatch alone would also give):
    // callA must have compiled, and its own inline_tally must show at least
    // one speculative (guarded) site admitted.
    let class_id = vm
        .shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id("cratonvm/PgoGuardedVirtualInline")
        .expect("class must be loaded after invoke");
    let compiled = vm
        .shared
        .jit
        .jit_cache
        .read()
        .get("cratonvm/PgoGuardedVirtualInline", "callA", "(I)I", class_id)
        .ok_or_else(|| "check_guard_hit: callA never JIT-compiled".to_string())?;
    // State the tier dependency instead of relying on it. `inline_tally` is a
    // single-pass artifact's record; an IR artifact leaves it zeroed, so
    // without this rung the assertion below cannot tell "the guard did not
    // fire" from "a different backend compiled the method and was never asked
    // to plan an inline". Those are opposite conclusions.
    if compiled.used_ir_backend {
        return Err(
            "check_guard_hit: callA was compiled by the OPTIMIZING (IR) backend, which \
             plans no guarded inlines and records no inline_tally — the pin in \
             `test_pgo02_guarded_virtual_inline` did not take"
                .to_string(),
        );
    }
    if compiled.inline_tally.speculative_sites == 0 {
        return Err(format!(
            "check_guard_hit: callA compiled but speculative_sites == 0              (tally={:?}) — the guard never actually fired, correctness above              only proves normal dispatch works",
            compiled.inline_tally
        ));
    }
    Ok(())
}

/// The critical correctness check: build a monomorphic-A profile (and, with
/// the flag on, an A-guarded compiled inline) on `callCurrent`, then switch
/// the receiver to three OTHER concrete classes without recompiling. Every
/// one of those later calls must dispatch to the actual runtime class's
/// `tag()`, not silently run A's already-inlined body against a receiver
/// the guard should have rejected.
fn check_guard_miss() -> Result<(), String> {
    let mut vm = test_vm();
    invoke_void(&mut vm, "setCurrent", &[Value::Int(0)]); // A
    for i in 0..CALLS {
        let got = invoke_int(&mut vm, "callCurrent", &[Value::Int(i)]);
        let want = i + 1;
        if got != want {
            return Err(format!(
                "check_guard_miss (A phase): callCurrent({i}) = {got}, want {want}"
            ));
        }
    }

    // The guard (if the flag admitted one) is now baked in for class A.
    // Switch the receiver and confirm every subsequent call still resolves
    // to the ACTUAL runtime class, not the guarded/inlined body.
    let cases: &[(i32, i32)] = &[(1, 1000), (2, 2000), (3, 3000)];
    for &(which, offset) in cases {
        invoke_void(&mut vm, "setCurrent", &[Value::Int(which)]);
        for i in 0..20 {
            let got = invoke_int(&mut vm, "callCurrent", &[Value::Int(i)]);
            let want = i + offset;
            if got != want {
                return Err(format!(
                    "check_guard_miss (which={which} phase): callCurrent({i}) = {got}, want {want} — \
                     a mismatched guard must fall back to normal dispatch, not silently run the wrong body"
                ));
            }
        }
    }
    Ok(())
}

/// A callee that throws, called past the compile threshold: verifies a
/// guard-eligible call site's exception + catch control flow is unchanged
/// once compiled.
fn check_thrower() -> Result<(), String> {
    let mut vm = test_vm();
    for i in 0..CALLS {
        let got = invoke_int(&mut vm, "callThrowerCaught", &[Value::Int(i)]);
        let want = if i == 7 { -1000 - i } else { i + 1 };
        if got != want {
            return Err(format!(
                "check_thrower: callThrowerCaught({i}) = {got}, want {want}"
            ));
        }
    }
    Ok(())
}

/// A 4-type call site with no dominant receiver: plan_inline must refuse to
/// speculate (Megamorphic / ReceiverNotDominant), and regardless of that
/// classification every call must still dispatch correctly — proving the
/// widened admission path doesn't corrupt a shape it was never meant to
/// guard.
fn check_polymorphic() -> Result<(), String> {
    let mut vm = test_vm();
    for i in 0..CALLS {
        let which = i % 4;
        let got = invoke_int(&mut vm, "callPoly", &[Value::Int(i), Value::Int(which)]);
        let want = i + [1, 1000, 2000, 3000][which as usize];
        if got != want {
            return Err(format!(
                "check_polymorphic: callPoly({i}, {which}) = {got}, want {want}"
            ));
        }
    }
    Ok(())
}
