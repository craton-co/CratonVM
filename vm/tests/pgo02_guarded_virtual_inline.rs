// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! PGO-02 (docs/feature-designs/profile-guided-inlining.md, retired from
//! pgo-02-guarded-inlining.md): guarded monomorphic
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
    match vm.invoke(
        "cratonvm/PgoGuardedVirtualInline",
        method,
        method_descriptor(method),
        args,
    ) {
        Ok(Some(Value::Int(n))) => n,
        other => panic!("{method} failed or returned a non-int: {other:?}"),
    }
}

fn invoke_void(vm: &mut Vm, method: &str, args: &[Value]) {
    match vm.invoke(
        "cratonvm/PgoGuardedVirtualInline",
        method,
        method_descriptor(method),
        args,
    ) {
        Ok(None) => {}
        other => panic!("{method} failed or returned a value: {other:?}"),
    }
}

fn method_descriptor(method: &str) -> &'static str {
    match method {
        "callA"
        | "callCurrent"
        | "callThrowerCaught"
        | "callOverride"
        | "callIface"
        | "callDivider"
        | "callDividerForTrace"
        | "dividerTagFrames"
        | "callFinally"
        | "callSynchronized"
        | "callMonitor" => "(I)I",
        "callPoly" | "callBimorphic" => "(II)I",
        "setCurrent" | "setDivisor" => "(I)V",
        "finallySideEffects" => "()I",
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
            // So `check_metrics_harvest` has something to read. The design doc
            // recorded the inline tally as unharvested; it is harvested, and
            // this is the run that proves it end to end rather than by reading
            // `metrics.rs`.
            ("CRATONVM_JIT_METRICS", Some("1")),
        ],
        run_all_checks,
    );
    enable_profiling(false);
    if let Err(msg) = result {
        panic!("{msg}");
    }
}

/// ONE `Vm` for every check.
///
/// This file used to build a fresh `Vm` per check. It turned out that only the
/// FIRST one ever compiled anything — the tiered background compile worker is
/// process-global and did not warm up again for later VMs — so every check
/// after `check_guard_hit` ran fully interpreted, including the guard-MISS
/// check this file calls its single most safety-critical one. They asserted
/// correct results, got them from the interpreter, and proved nothing about
/// the compiled path. Found by `CRATONVM_DBG_JITC=1`: exactly one
/// `bg-compile` line in the whole run.
///
/// Every check that depends on a compiled artifact now ASSERTS that the
/// artifact exists (`compiled_tally`), so the same silent regression to
/// "correct, but interpreted" fails loudly instead of passing.
fn run_all_checks() -> Result<(), String> {
    let mut vm = test_vm();
    check_guard_hit(&mut vm)?;
    check_guard_miss(&mut vm)?;
    check_override_receiver(&mut vm)?;
    check_interface_site(&mut vm)?;
    check_bimorphic(&mut vm)?;
    check_uncaught_from_inlined_frame(&mut vm)?;
    check_stack_trace_through_an_inlined_frame(&mut vm)?;
    check_monitor_bearing_callees_are_refused(&mut vm)?;
    check_finally_runs_at_a_guard_eligible_site(&mut vm)?;
    check_thrower(&mut vm)?;
    check_polymorphic(&mut vm)?;
    check_metrics_harvest()?;
    Ok(())
}

/// The inlining tally reaches the metrics surface.
///
/// `docs/feature-designs/profile-guided-inlining.md` recorded this as an open
/// gap ("Nothing in `jit/src/metrics.rs` harvests it yet"). It does —
/// `CompileRecorder::installed` copies the whole tally off the artifact — but
/// the claim was only ever checked by reading the source, and a harvest that
/// runs on no real compile is indistinguishable from one that does not exist.
/// This asserts it against the reports the compiles above actually published.
fn check_metrics_harvest() -> Result<(), String> {
    let reports = cratonvm_jit::metrics::compilation_reports();
    if reports.is_empty() {
        return Err(
            "check_metrics_harvest: no compilation reports at all — CRATONVM_JIT_METRICS did              not take, so nothing below would have been measured"
                .to_string(),
        );
    }
    let speculative = reports
        .iter()
        .filter(|r| matches!(r.speculative_inlined_sites, cratonvm_jit::metrics::Measured::Value(n) if n > 0))
        .count();
    if speculative == 0 {
        let measured = reports
            .iter()
            .filter(|r| {
                !matches!(
                    r.inline_candidates,
                    cratonvm_jit::metrics::Measured::NotMeasured
                )
            })
            .count();
        return Err(format!(
            "check_metrics_harvest: {} reports, {measured} carrying an inline tally, but none              reporting a speculative site — the guarded inlines the checks above proved              happened did not reach the metrics surface",
            reports.len()
        ));
    }
    Ok(())
}

/// Two receiver classes, both overriding, in an even mix — the Bimorphic
/// verdict, and the two-guard chain that emits it.
///
/// Results alone cannot tell a two-guard site from a one-guard site: with only
/// the first guard emitted, a `C` receiver simply misses and dispatches, which
/// is also correct. So this checks the SPLICED BYTE COUNT. `B.tag` and `C.tag`
/// are 6 bytecodes each (`iload_1; sipush; iadd; ireturn`), so a bimorphic
/// splice charges 12 and a monomorphic one 6.
fn check_bimorphic(vm: &mut Vm) -> Result<(), String> {
    for i in 0..CALLS {
        let which = i % 2;
        let got = invoke_int(vm, "callBimorphic", &[Value::Int(i), Value::Int(which)]);
        let want = i + if which == 0 { 1000 } else { 2000 };
        if got != want {
            return Err(format!(
                "check_bimorphic: callBimorphic({i}, {which}) = {got}, want {want}"
            ));
        }
    }
    let tally = compiled_tally(vm, "callBimorphic", &[Value::Int(1), Value::Int(0)])?;
    if tally.speculative_sites == 0 {
        return Err(format!(
            "check_bimorphic: callBimorphic compiled but speculative_sites == 0 (tally={tally:?})"
        ));
    }
    if tally.inlined_bytecodes < 12 {
        return Err(format!(
            "check_bimorphic: only {} callee bytecodes spliced (tally={tally:?}) — a Bimorphic \
             verdict must splice BOTH receiver classes' bodies (6 + 6); one body means the \
             second guard was never emitted and the site degraded to monomorphic",
            tally.inlined_bytecodes
        ));
    }
    Ok(())
}

/// §8 item 7: an UNCAUGHT exception propagating out of a guard-hit inlined
/// frame.
///
/// The callee divides by an instance field. Warm up with a non-zero divisor so
/// the site compiles with a guard baked in, then set the divisor to 0: the
/// spliced `idiv` raises ArithmeticException inside an inlined frame, with no
/// handler in that frame and none in the caller either.
///
/// The control is the SAME call before the method was ever compiled — if the
/// compiled path disagrees with the interpreter about what escapes, that is
/// the bug this check exists for, and comparing against a hard-coded string
/// would only prove the compiler agrees with the test author.
fn check_uncaught_from_inlined_frame(vm: &mut Vm) -> Result<(), String> {
    // Interpreted control, before any warmup of this entry point.
    invoke_void(vm, "setDivisor", &[Value::Int(0)]);
    let interpreted = vm.invoke(
        "cratonvm/PgoGuardedVirtualInline",
        "callDivider",
        "(I)I",
        &[Value::Int(9)],
    );
    let interpreted = match interpreted {
        Err(e) => describe_failure(vm, &e),
        Ok(v) => {
            return Err(format!(
                "check_uncaught_from_inlined_frame: interpreted callDivider(9) with divisor 0 \
                 returned {v:?} instead of raising — the fixture does not divide by zero, so \
                 the compiled comparison below would be vacuous"
            ))
        }
    };

    // Warm up past the compile threshold with a safe divisor.
    invoke_void(vm, "setDivisor", &[Value::Int(1)]);
    for i in 0..CALLS {
        let got = invoke_int(vm, "callDivider", &[Value::Int(i)]);
        if got != i {
            return Err(format!(
                "check_uncaught_from_inlined_frame: callDivider({i}) = {got}, want {i}"
            ));
        }
    }
    let tally = compiled_tally(vm, "callDivider", &[Value::Int(1)])?;

    // Now the same division raises, from inside whatever the compiled body is.
    invoke_void(vm, "setDivisor", &[Value::Int(0)]);
    let compiled = vm.invoke(
        "cratonvm/PgoGuardedVirtualInline",
        "callDivider",
        "(I)I",
        &[Value::Int(9)],
    );
    let compiled = match compiled {
        Err(e) => describe_failure(vm, &e),
        Ok(v) => {
            return Err(format!(
                "check_uncaught_from_inlined_frame: compiled callDivider(9) with divisor 0 \
                 returned {v:?} — the exception was swallowed (tally={tally:?})"
            ))
        }
    };
    if compiled != interpreted {
        return Err(format!(
            "check_uncaught_from_inlined_frame: compiled and interpreted disagree about the \
             escaping exception.\n  interpreted: {interpreted}\n  compiled:    {compiled}\n  \
             (tally={tally:?})"
        ));
    }
    // State which path was actually exercised. A refusal here is a legitimate
    // outcome — `try_emit_inline`'s deopt-metadata postcondition refuses any
    // body that publishes a deopt point, and an inlined `idiv` may do exactly
    // that — but it means this check covered the DISPATCH path, not the
    // spliced one, and that difference must not be silent.
    if tally.speculative_sites == 0 {
        eprintln!(
            "[pgo02] note: callDivider was not spliced (tally={tally:?}); the uncaught-exception \
             check above exercised the guard-miss/dispatch path. See \
             docs/feature-designs/profile-guided-inlining.md §8."
        );
    }
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
fn check_override_receiver(vm: &mut Vm) -> Result<(), String> {
    for i in 0..CALLS {
        let got = invoke_int(vm, "callOverride", &[Value::Int(i)]);
        let want = i + 1000;
        if got != want {
            return Err(format!(
                "check_override_receiver: callOverride({i}) = {got}, want {want} (call #{i} of \
                 {CALLS}) — the guard admitted a receiver of class B and ran a body that is not \
                 B's `tag`"
            ));
        }
    }
    // The results above come from the interpreter unless this holds, and the
    // interpreter was never the thing at risk.
    let tally = compiled_tally(vm, "callOverride", &[Value::Int(1)])?;
    if tally.speculative_sites == 0 {
        return Err(format!(
            "check_override_receiver: callOverride compiled but speculative_sites == 0              (tally={tally:?}) — the guarded path never ran, so a wrong spliced body              would not have been observable"
        ));
    }
    Ok(())
}

/// `invokeinterface` with a single implementation. Resolution that starts at
/// the constant-pool class finds `Tagger.itag`'s ABSTRACT declaration, which
/// has no `Code` attribute, so the site can never be spliced; resolution from
/// the speculated receiver class finds `OnlyImpl.itag` and can. Asserts both
/// the result and that a speculative site was actually admitted, so a
/// regression back to "correct but never inlined" is visible.
fn check_interface_site(vm: &mut Vm) -> Result<(), String> {
    for i in 0..CALLS {
        let got = invoke_int(vm, "callIface", &[Value::Int(i)]);
        let want = i + 77;
        if got != want {
            return Err(format!(
                "check_interface_site: callIface({i}) = {got}, want {want}"
            ));
        }
    }
    let tally = compiled_tally(vm, "callIface", &[Value::Int(1)])?;
    if tally.speculative_sites == 0 {
        return Err(format!(
            "check_interface_site: callIface compiled but speculative_sites == 0 (tally={tally:?}) \
             — an interface site with one implementation must be reachable by the guarded \
             inliner"
        ));
    }
    Ok(())
}

/// The brief's stack-trace requirement: "a guard that always fires must produce
/// the same observable results as the un-inlined path — same exceptions, same
/// STACK TRACES, same `finally` execution".
///
/// A spliced body has no frame of its own, and `capture_current_stack_trace`
/// walks the interpreter's frame list, so the question is whether the callee
/// still appears. Measured against the SAME call before the method compiled.
///
/// 2026-09-05: that last sentence was false, and this check said so itself,
/// once every five runs. It drove `callDivider`, which
/// `check_uncaught_from_inlined_frame` has already compiled and spliced by the
/// time this runs, so the "interpreted" reading was a second COMPILED reading
/// and the comparison below was the compiled path against itself. Its own
/// `interpreted == 0` floor is what caught it. The entry point is now
/// `callDividerForTrace`, which nothing else drives — see the fixture's
/// comment on it. Two checks sharing one entry point and one `Vm` is the
/// hazard; the floor is what made it visible rather than vacuous.
fn check_stack_trace_through_an_inlined_frame(vm: &mut Vm) -> Result<(), String> {
    // Read the trace the VM CAPTURED, not one reconstructed through Java
    // reflection: these in-process tests boot the synthetic JDK, whose
    // `Throwable` has no `getStackTrace()` (it raises NoSuchMethodError). The
    // captured `Vec<StackTraceEntry>` is what `Throwable.getStackTrace()` is
    // served from, so this reads the same structure the consumer does.
    fn tag_frames(vm: &mut Vm, x: i32) -> Result<usize, String> {
        let err = match vm.invoke(
            "cratonvm/PgoGuardedVirtualInline",
            "callDividerForTrace",
            "(I)I",
            &[Value::Int(x)],
        ) {
            Err(e) => e,
            Ok(v) => {
                return Err(format!(
                    "callDividerForTrace({x}) returned {v:?} instead of raising — the fixture is                      not dividing by zero, so this check would be vacuous"
                ))
            }
        };
        let cratonvm_vm::error::MethodCallFailed::ExceptionThrown(exc) = err else {
            return Err(format!(
                "callDividerForTrace({x}) failed without a throwable: {err:?}"
            ));
        };
        let hash = vm.shared.mem.heap.identity_hash_code(exc);
        let trace = vm
            .shared
            .throwable_stack_trace(hash)
            .ok_or_else(|| "no stack trace was captured for the throwable".to_string())?;
        Ok(trace.iter().filter(|f| &*f.method_name == "tag").count())
    }

    invoke_void(vm, "setDivisor", &[Value::Int(0)]);
    let interpreted = tag_frames(vm, 9)?;

    invoke_void(vm, "setDivisor", &[Value::Int(1)]);
    for i in 0..CALLS {
        let _ = invoke_int(vm, "callDividerForTrace", &[Value::Int(i)]);
    }
    let tally = compiled_tally(vm, "callDividerForTrace", &[Value::Int(1)])?;
    if tally.speculative_sites == 0 {
        return Err(format!(
            "check_stack_trace_through_an_inlined_frame: callDividerForTrace was not spliced              ({tally:?}), so this check would compare the dispatch path against itself"
        ));
    }

    invoke_void(vm, "setDivisor", &[Value::Int(0)]);
    let compiled = tag_frames(vm, 9)?;
    eprintln!("[pgo02] tag frames: interpreted={interpreted} compiled={compiled}");

    // A floor, or the comparison below is vacuous: two zeros agree perfectly
    // and say nothing about whether the callee frame survives inlining.
    if interpreted == 0 {
        return Err(
            "check_stack_trace_through_an_inlined_frame: the INTERPRETED trace names no              `tag` frame at all, so comparing it against the compiled one proves nothing.              The fixture or the capture path changed."
                .to_string(),
        );
    }

    if compiled != interpreted {
        return Err(format!(
            "check_stack_trace_through_an_inlined_frame: the captured stack trace of an              exception raised inside a GUARD-HIT INLINED body names {compiled} `tag`              frame(s); the interpreted path names {interpreted}. The pgo-02 brief requires              a guard that always fires to produce \"the same observable results as the              un-inlined path — same exceptions, same stack traces, same `finally`              execution\". A spliced body has no frame of its own and              `deopt::FrameState::caller` is populated by nobody, so there is nothing to              rebuild the callee frame from (tally={tally:?})."
        ));
    }
    Ok(())
}

/// The brief's second blocker: every `FrameState` the lowerer builds hard-codes
/// an EMPTY MONITOR LIST, so an inlined body can carry no monitor state. "If
/// this lane inlines across a monitor, it must keep that refusal."
///
/// Both shapes are covered — a `synchronized` method and a `synchronized`
/// block, which are different bytecode (an access flag vs.
/// `monitorenter`/`monitorexit`). The refusal exists in the resolver; nothing
/// pinned it, and a refusal nobody tests is a refusal that can be relaxed by
/// accident.
fn check_monitor_bearing_callees_are_refused(vm: &mut Vm) -> Result<(), String> {
    for (method, offset) in [("callSynchronized", 42), ("callMonitor", 99)] {
        for i in 0..CALLS {
            let got = invoke_int(vm, method, &[Value::Int(i)]);
            if got != i + offset {
                return Err(format!("{method}({i}) = {got}, want {}", i + offset));
            }
        }
        let tally = compiled_tally(vm, method, &[Value::Int(1)])?;
        if tally.speculative_sites != 0 {
            return Err(format!(
                "check_monitor_bearing_callees_are_refused: {method} spliced a                  monitor-bearing callee ({tally:?}). An inlined body's FrameState carries                  no monitor list, so there would be nothing to rebuild at a deopt."
            ));
        }
        // Pin WHY. Without this the check also passes when the site was
        // refused for an unrelated reason — an unprofiled site refuses too,
        // and that would be a different fact wearing the same result.
        if tally.refusal_count("callee-unresolved") == 0 {
            return Err(format!(
                "check_monitor_bearing_callees_are_refused: {method} did not splice, but                  not because the monitor-bearing callee was refused ({tally:?}). The                  refusal this check exists for is `callee-unresolved`, raised by the                  resolver's monitorenter/synchronized rejection."
            ));
        }
    }
    Ok(())
}

/// The brief's `finally` requirement, and the reason it is called out: "this VM
/// has already shipped a JIT-compiled `finally` that was not run on three
/// escape routes; inlining multiplies that surface."
///
/// The resolver refuses any callee with a non-empty exception table, so a
/// `finally`-bearing callee is never spliced — but that is a claim about the
/// resolver, and what matters is that the `finally` RUNS, once per call, on
/// both escape routes, at a site the guard was otherwise eligible for.
fn check_finally_runs_at_a_guard_eligible_site(vm: &mut Vm) -> Result<(), String> {
    let before = invoke_int(vm, "finallySideEffects", &[]);
    let mut expected = 0;
    for i in 0..CALLS {
        let got = invoke_int(vm, "callFinally", &[Value::Int(i)]);
        let want = if i == 13 { -13 } else { i + 1 };
        expected += 1;
        if got != want {
            return Err(format!("callFinally({i}) = {got}, want {want}"));
        }
    }
    // Read the counter HERE, before `compiled_tally`. That helper keeps CALLING
    // the method while it waits for the background worker — up to 400 rounds of
    // 50 — and every one of those calls runs the `finally` too, while `expected`
    // counts only this function's own loop. So the assertion below held exactly
    // when the artifact happened to be installed by the first poll, and read
    // "the `finally` ran 900 times for 700 calls" when it took four rounds: a
    // 5.6%-of-runs failure that accused the VM of double-executing a `finally`
    // when the extra runs were the test's own calls. (2026-09-05. It surfaced
    // once the stack-trace check above stopped failing first and aborting the
    // run before this one; a flake can hide a flake.)
    let after = invoke_int(vm, "finallySideEffects", &[]);
    let tally = compiled_tally(vm, "callFinally", &[Value::Int(1)])?;
    if tally.speculative_sites != 0 {
        return Err(format!(
            "check_finally_runs_at_a_guard_eligible_site: callFinally spliced a callee              carrying an exception table ({tally:?})"
        ));
    }
    if after - before != expected {
        return Err(format!(
            "check_finally_runs_at_a_guard_eligible_site: the `finally` ran {} times for              {expected} calls (both escape routes must run it, exactly once each)",
            after - before
        ));
    }
    Ok(())
}

/// Name a failed call by what ESCAPED, not by the heap address it escaped in.
///
/// Comparing the raw `ObjectRef` compares two allocations of the same
/// exception and always differs; the question this file asks is whether the
/// compiled path and the interpreter throw the same THING.
fn describe_failure(vm: &Vm, failure: &cratonvm_vm::error::MethodCallFailed) -> String {
    match failure {
        cratonvm_vm::error::MethodCallFailed::ExceptionThrown(exc) => {
            let class_id = vm.shared.mem.heap.class_id_of(*exc);
            let class_name = vm
                .shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .map(|class| class.name.to_string())
                .unwrap_or_else(|| format!("<unknown class {class_id:?}>"));
            format!("ExceptionThrown({class_name})")
        }
        other => format!("{other:?}"),
    }
}

/// The `inline_tally` of a compiled entry point, with the tier dependency
/// stated rather than relied on (an IR artifact leaves the tally zeroed, which
/// is the opposite conclusion from "the guard did not fire").
fn compiled_tally(
    vm: &mut Vm,
    method: &str,
    warm_args: &[Value],
) -> Result<cratonvm_jit::InlineDecisionTally, String> {
    // Compilation is ASYNCHRONOUS. `CRATONVM_DBG_JITC=1` shows it as
    // `bg-compile … bg=true`: crossing the invocation threshold enqueues the
    // method, and a background worker installs the artifact some time later.
    // A release build runs this file's 700-call warm-up in ~80 ms, which is
    // routinely faster than the worker — so reading the cache once and
    // declaring "never JIT-compiled" is a race, not a result. It passed on
    // Windows/debug (a slow enough interpreter that the worker always won)
    // and failed on Linux/release inside the full suite.
    //
    // Keep calling the method while waiting. That gives the worker both the
    // trigger and the time, and it is what a real caller would be doing
    // anyway. The bound is generous because a loaded CI host is exactly when
    // the worker is slowest; exceeding it is still a failure, because
    // "eventually compiles" is the claim under test.
    const ATTEMPTS: usize = 400;
    const CALLS_PER_ATTEMPT: i32 = 50;

    for attempt in 0..ATTEMPTS {
        let class_id = vm
            .shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id("cratonvm/PgoGuardedVirtualInline")
            .ok_or_else(|| "class must be loaded after invoke".to_string())?;
        let found = vm.shared.jit.jit_cache.read().get(
            "cratonvm/PgoGuardedVirtualInline",
            method,
            method_descriptor(method),
            class_id,
        );
        if let Some(compiled) = found {
            // State the tier dependency instead of relying on it.
            // `inline_tally` is a single-pass artifact's record; an IR
            // artifact leaves it zeroed, so without this rung "the guard did
            // not fire" and "a different backend compiled the method" are
            // indistinguishable — and they are opposite conclusions.
            if compiled.used_ir_backend {
                return Err(format!(
                    "{method} was compiled by the OPTIMIZING (IR) backend, which plans no \
                     guarded inlines and records no inline_tally — the tier pin did not take"
                ));
            }
            return Ok(compiled.inline_tally.clone());
        }
        if attempt + 1 == ATTEMPTS {
            break;
        }
        for i in 0..CALLS_PER_ATTEMPT {
            let _ = invoke_int(vm, method, warm_args);
            let _ = i;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    Err(format!(
        "{method} never JIT-compiled, after {} further calls and ~{} ms of waiting",
        ATTEMPTS as i32 * CALLS_PER_ATTEMPT,
        ATTEMPTS * 5
    ))
}

/// Positive control: a fixed monomorphic-A call site, called past the
/// (lowered) JIT threshold, must keep returning the correct result once
/// compiled — the guard-hit path must be observably identical to the
/// interpreted / unguarded-dispatch result.
fn check_guard_hit(vm: &mut Vm) -> Result<(), String> {
    for i in 0..CALLS {
        let got = invoke_int(vm, "callA", &[Value::Int(i)]);
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
    let tally = compiled_tally(vm, "callA", &[Value::Int(1)])?;
    if tally.speculative_sites == 0 {
        return Err(format!(
            "check_guard_hit: callA compiled but speculative_sites == 0 (tally={tally:?}) \
             — the guard never actually fired, so the correctness above only proves \
             normal dispatch works"
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
fn check_guard_miss(vm: &mut Vm) -> Result<(), String> {
    invoke_void(vm, "setCurrent", &[Value::Int(0)]); // A
    for i in 0..CALLS {
        let got = invoke_int(vm, "callCurrent", &[Value::Int(i)]);
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
    // Without this the phases below run interpreted and the check is empty:
    // "a mismatched guard falls back to dispatch" is only a claim about
    // COMPILED code.
    let tally = compiled_tally(vm, "callCurrent", &[Value::Int(1)])?;
    if tally.speculative_sites == 0 {
        return Err(format!(
            "check_guard_miss: callCurrent compiled but speculative_sites == 0              (tally={tally:?}) — no guard was baked in, so switching the receiver              tests nothing"
        ));
    }

    let cases: &[(i32, i32)] = &[(1, 1000), (2, 2000), (3, 3000)];
    for &(which, offset) in cases {
        invoke_void(vm, "setCurrent", &[Value::Int(which)]);
        for i in 0..20 {
            let got = invoke_int(vm, "callCurrent", &[Value::Int(i)]);
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
fn check_thrower(vm: &mut Vm) -> Result<(), String> {
    for i in 0..CALLS {
        let got = invoke_int(vm, "callThrowerCaught", &[Value::Int(i)]);
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
fn check_polymorphic(vm: &mut Vm) -> Result<(), String> {
    for i in 0..CALLS {
        let which = i % 4;
        let got = invoke_int(vm, "callPoly", &[Value::Int(i), Value::Int(which)]);
        let want = i + [1, 1000, 2000, 3000][which as usize];
        if got != want {
            return Err(format!(
                "check_polymorphic: callPoly({i}, {which}) = {got}, want {want}"
            ));
        }
    }
    Ok(())
}
