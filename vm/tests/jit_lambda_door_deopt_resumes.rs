// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The LAMBDA door's half of "a trapping compiled body must not be re-run from
//! entry" — and the reason its sibling file could not assert it.
//!
//! # The gap this closes
//!
//! `jit_bridge_sink_resumes_instead_of_rerunning.rs` fixed and pinned three
//! sinks, and for the third one — `resume_deopted_body`, shared by
//! `execute_jit_call_oneshot` and the compiled caller's
//! `try_lambda_site_direct_call` — it reaches for the counter whose own doc
//! calls it *"the metric a regression test asserts is zero"*:
//!
//! ```ignore
//! let (_resumed, unresumable) = lambda_site_deopt_outcomes();
//! assert_eq!(unresumable, 0, "...");
//! ```
//!
//! **That zero is structural.** Its fixture, `DeoptRerunCount`, contains no SAM
//! anywhere — it is `invokestatic` throughout — so neither `SITE_RESUMED` nor
//! `SITE_UNRESUMABLE` can be incremented by it. The assertion holds just as
//! firmly on a VM whose lambda door is completely broken, because the counter
//! was never touched. A zero from a counter nothing bumped is evidence about
//! the counter.
//!
//! That file's own comment says the fixture "does not otherwise reach" this
//! sink, so the gap is acknowledged rather than hidden; this file is the
//! fixture that does reach it.
//!
//! # What makes it not vacuous
//!
//! Two assertions, and the ORDER matters: engagement first.
//!
//! * `resumed > 0` — the lambda door actually resumed a trapped body in this
//!   run. Without this, everything below is a statement about zero.
//! * `unresumable == 0` — none was answered by a re-run from entry.
//! * `delta == 1` — and the observable agrees: the side effect ran once for one
//!   call. `2` would mean the impl was re-entered from bci 0.
//!
//! The third is the one that would catch a resume that *reported* success and
//! rebuilt the frame wrongly, which the two counters cannot see.
//!
//! # Everything goes through Java, warm-up included
//!
//! For the same two reasons the sibling file documents at length: `vm.invoke`
//! enters `execute`, whose own tier-up sink resumes correctly (so calling the
//! impl from Rust would measure the wrong sink), and whose first-call path
//! caches a SINGLE-PASS body that preempts the optimizing pipeline (so warming
//! the impl directly would pin it at C1, where `can_deopt_resume` IS set and
//! the defect is invisible). `warm()` and `trip()` are Java; the impl is only
//! ever reached through `IntOp.apply`.
//!
//! # And the SAM call site has to be compiled too
//!
//! `try_lambda_site_direct_call` is entered from COMPILED CODE only —
//! `jit_invoke_virtual_mic` and the sibling door in `jit/helpers.rs` — never
//! from an interpreted frame. A first version of this file put
//! `OP.apply(1, 0)` straight into `trip()`, which runs interpreted; the impl
//! WAS compiled and DID trap, but the deopt went to the ordinary interface
//! sink and both lambda-site counters stayed at zero. The engagement assertion
//! below caught that on the first run, which is the whole reason it is there.
//! `DeoptLambdaRerunCount.step` exists to be the compiled call site.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

const CLASS: &str = "cratonvm/DeoptLambdaRerunCount";

/// The lambda BODY, which is what the optimizing tier compiles and what traps.
/// `javac` emits `(i, d) -> { … }` in a static-field initializer as this
/// synthetic private static method; if a compiler change ever renames it, the
/// anti-vacuity check below fails loudly rather than passing on the wrong
/// method.
const IMPL: &str = "lambda$static$0";
const IMPL_DESC: &str = "(II)I";

/// The SAM CALL SITE. It has to be compiled too — see
/// `wait_until_optimizing_compiled`.
const SITE: &str = "step";
const SITE_DESC: &str = "(I)I";

/// `warm(ITERS_PER_WARM)` per outer call; the PRODUCT is what has to clear
/// `TIER_OVERRIDES`' C2 threshold, since each inner iteration is one dispatch
/// of the impl.
const WARM_CALLS: i32 = 200;
const ITERS_PER_WARM: i32 = 64;

/// Identical to the sibling file's, and for its reasons: take the
/// `Interpreter -> C2` door rather than waiting for a C1 body to be superseded
/// (at C1 the single-pass backend sets `can_deopt_resume` itself and the
/// pre-existing arm resumes, hiding the defect), and then KEEP the optimizing
/// body — a method simple enough to be a clean witness for one store and one
/// trap is by construction too simple to earn its keep on evidence.
const TIER_OVERRIDES: &[(&str, Option<&str>)] = &[
    ("CRATONVM_TIER_C1_THRESHOLD", Some("100000")),
    ("CRATONVM_TIER_C2_THRESHOLD", Some("600")),
    ("CRATONVM_TIER_C2_MIN_INVOCATIONS", Some("500")),
    ("CRATONVM_C2_ACCEPT", Some("always")),
];

/// JIT r23 wave 11 (lane L2): keep the two DRIVERS (`warm`, `warmStatic`)
/// interpreted; `step`, the impl, `stepStatic` and `implStatic` still compile.
///
/// `trip` runs interpreted and calls `step` once. That one call enters the
/// compiled `step` only through this thread's invoke-cache entry for the
/// `step` Methodref, which `trip` shares with `warm` (the cache is keyed by
/// caller class and constant-pool index). A miss takes the slow path, and
/// `try_stackless_invoke` runs the callee INTERPRETED: `step`'s
/// `invokeinterface` then goes through the interpreter's lambda door
/// (`invoke_fast::execute_lambda_door`, sink `jit-callsite-b`), which resumes
/// the trap precisely (`delta=1`) but is not the compiled caller's door this
/// test counts (`resumed=0`). Measured with `CRATONVM_INVOKE_CACHE_PC_KEY=1`,
/// which gives `trip` its own entry: 6 failures in 6 runs.
///
/// Compiled, `warm` makes that miss. Every optimizing publication advances
/// the VM's supersede epoch (`c2-supersede ... epoch_bumped=true`, first
/// publications included), and `InvokeCache::get` evicts every `Jit` entry
/// filled before it. When `warm`'s own body lands after its last interpreted
/// call of `step`, nothing refills the entry before `trip`. Under a 24-thread
/// CPU hog on the 80a5181462 test exe: 6 of 40 and 5 of 40 failed (the second
/// with every wave-10 default the brief suspected set to `0`), always
/// `resumed=0 unresumable=0 | delta=1`; with `warm` denied, 0 of 40, and 0 of
/// 20 whole-binary runs. Unloaded it passes every time.
///
/// The first-publish bump itself is fixed in the runtime
/// (`CRATONVM_JIT_SUPERSEDE_EPOCH_ONLY_ON_REPLACE`, default ON; page
/// `r23-w11-first-publish-evicts-every-jit-invoke-cache-entry-FIXED-20261023.md`).
/// The pin stays because any real replacement still bumps the epoch, such as
/// `warm`'s body being republished, and the arm should not depend on when
/// that lands.
///
/// What the arm measures is unchanged: a compiled `step` calls the compiled
/// impl through the lambda site, the impl traps, and the site's door resumes
/// it. An interpreted `warm` reaches the compiled `step` through the same
/// invoke-cache entry `trip` then uses, and keeps it current. Both tests set
/// it: `CRATONVM_JIT_DENY` is parsed once per process.
const DRIVER_OVERRIDES: &[(&str, Option<&str>)] =
    &[("CRATONVM_JIT_DENY", Some("cratonvm/DeoptLambdaRerunCount.warm"))];

fn test_resources_dir() -> String {
    if let Some(generated) = option_env!("CRATONVM_TEST_CLASSES_DIR") {
        if std::path::Path::new(generated).is_dir() {
            return generated.to_owned();
        }
    }
    format!("{}/tests/resources", env!("CARGO_MANIFEST_DIR"))
}

fn assert_fixture_staged() {
    let path = format!(
        "{}/cratonvm/DeoptLambdaRerunCount.class",
        test_resources_dir()
    );
    assert!(
        std::path::Path::new(&path).exists(),
        "fixture not staged at {path} — rebuild with `javac -d vm/tests/resources \
         vm/tests/resources/cratonvm/DeoptLambdaRerunCount.java` and commit BOTH \
         .class files (the SAM interface gets its own)"
    );
}

/// Wait for BOTH artifacts this file needs, driving them from BYTECODE.
///
/// * the IMPL, on the OPTIMIZING backend. The backend is checked, not merely
///   that an artifact exists: a single-pass body sets `can_deopt_resume` and
///   the sink's pre-existing arm resumes it, so a green run against one would
///   assert nothing about this defect.
/// * the CALL SITE (`step`), on any backend. This one is easy to forget and a
///   first draft did: `try_lambda_site_direct_call` is entered from compiled
///   code, so if `step` is still interpreted when the trap fires the deopt goes
///   to the ordinary interface sink and both lambda-site counters stay at zero
///   — which is precisely what the engagement assertion then reported.
fn wait_until_optimizing_compiled(vm: &mut Vm) {
    const ATTEMPTS: usize = 400;
    const CALLS_PER_ATTEMPT: i32 = 50;
    let mut saw_impl = false;
    let mut saw_site = false;
    for _ in 0..ATTEMPTS {
        let class_id = vm
            .shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(CLASS)
            .expect("the fixture class must be loaded after invoke");
        let (impl_ir, site_any) = {
            let cache = vm.shared.jit.jit_cache.read();
            let impl_ir = match cache.get(CLASS, IMPL, IMPL_DESC, class_id) {
                Some(c) => {
                    saw_impl = true;
                    c.used_ir_backend
                }
                None => false,
            };
            let site_any = cache.get(CLASS, SITE, SITE_DESC, class_id).is_some();
            saw_site |= site_any;
            (impl_ir, site_any)
        };
        if impl_ir && site_any {
            return;
        }
        for _ in 0..CALLS_PER_ATTEMPT {
            let _ = vm.invoke(CLASS, "warm", "(I)I", &[Value::Int(ITERS_PER_WARM)]);
        }
    }
    panic!(
        "the two artifacts this file needs were not both installed: \
         {CLASS}.{IMPL}{IMPL_DESC} on the OPTIMIZING backend was {}, and the \
         compiled call site {CLASS}.{SITE}{SITE_DESC} was {}. Proceeding would \
         measure the single-pass backend, or send the deopt to the ordinary \
         interface sink instead of the lambda door, and pass for the wrong reason",
        if saw_impl {
            "installed, but from the single-pass door"
        } else {
            "never installed"
        },
        if saw_site {
            "installed"
        } else {
            "never installed"
        },
    );
}

/// JIT r23 wave 6 (lane L2;
/// `r23-w5-lambda-door-resume-test-intermittently-never-enters-door-FIXED-20261021`): the two
/// tests in this binary install DIFFERENT process-scoped overrides
/// (`CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE=0` here, `=1` in the sibling), and
/// `cargo test` runs them on parallel threads. `with_process_overrides` leaves
/// serialisation to the caller ("concurrent installs are not supported"), so
/// the door test's VM compiled `step` under the sibling's `SPLICE=1` in about
/// one run in three: the SAM call was spliced, the trap resumed through the
/// splice's chain, and the door's counters never moved (`resumed=0`,
/// `delta=1`); `resumed=2` was the converse. Measured on the fe64a9a5c8 test
/// exe: 4 failures in 12 whole-binary runs, 0 in 6 runs of this test alone.
/// Every test here takes this lock first.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// The regression test for
/// `lambda-callee-deopt-is-orphaned-by-the-sam-name-check-20260908`, which was
/// `#[ignore]`d while that defect was open: 12 runs, 12 times `delta=2` for the
/// lambda arm against `delta_static=1` for the identical non-lambda control in
/// the same process. `try_resume_trapped_callee` now asks the metafactory's
/// `impl_handle` for the identity the SAM's name could not supply, and both
/// arms report 1.
///
/// It was checked in ahead of the fix rather than left as a paragraph on that
/// page because the hour that went into it was not the diagnosis, it was
/// getting the fixture to engage at all: the SAM call site has to be compiled,
/// the arm has to run before the control warms it, and the impl has to be on
/// the optimizing backend. All three are encoded here.
///
/// JIT round 23 wave 3b (lane L2): since the lambda SAM splice
/// (`CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE`, with the proxy receiver profile of
/// `CRATONVM_JIT_PROFILE_LAMBDA_RECEIVERS`), the optimizing `step` splices the
/// proxy's forwarder and the impl behind an exact proxy-class guard, so the
/// trap is resumed through the splice's deopt chain and the lambda-site door
/// this test counts is never entered (`resumed=0 unresumable=0 | delta=1`: the
/// observable stayed right). This test pins the splice OFF to keep testing the
/// door; [`a_spliced_lambda_trap_runs_its_side_effect_once`] is the same
/// fixture with the splice on.
#[test]
fn a_lambda_door_trap_resumes_and_runs_its_side_effect_once() {
    let _serial = serial();
    assert_fixture_staged();
    let mut overrides = TIER_OVERRIDES.to_vec();
    overrides.push(("CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE", Some("0")));
    // JIT r23 wave 7: with `CRATONVM_JIT_IR_KEEP_LAMBDA_SITE_CALLER` default ON
    // the caller keeps `step`'s own optimizing body, and in 3 of 12 runs
    // (`--test-threads=1`, n23-w6) the door was never entered; 0 of 12 with it
    // pinned off.
    overrides.push(("CRATONVM_JIT_IR_KEEP_LAMBDA_SITE_CALLER", Some("0")));
    overrides.extend_from_slice(DRIVER_OVERRIDES);
    cratonvm_types::flags::with_process_overrides(&overrides, body);
}

/// JIT round 23 wave 3b (lane L2): the same trap with the lambda SAM splice ON
/// (its default): the impl body is spliced into the compiled caller through
/// the proxy's forwarder, and a trap inside it resumes the impl's frame parked
/// in the caller's `invoke` (`IrBuilder::splice_scope_chain`'s forwarder skip).
/// The side effect must run exactly once, and the static control must agree.
#[test]
fn a_spliced_lambda_trap_runs_its_side_effect_once() {
    let _serial = serial();
    assert_fixture_staged();
    let mut overrides = TIER_OVERRIDES.to_vec();
    overrides.push(("CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE", Some("1")));
    overrides.push(("CRATONVM_JIT_PROFILE_LAMBDA_RECEIVERS", Some("1")));
    overrides.extend_from_slice(DRIVER_OVERRIDES);
    cratonvm_types::flags::with_process_overrides(&overrides, || {
        let mut vm = Vm::new(VmConfig::new().with_classpath(vec![test_resources_dir()]));
        for _ in 0..WARM_CALLS {
            let _ = vm.invoke(CLASS, "warm", "(I)I", &[Value::Int(ITERS_PER_WARM)]);
        }
        wait_until_optimizing_compiled(&mut vm);
        let delta = match vm.invoke(CLASS, "trip", "()I", &[]) {
            Ok(Some(Value::Int(v))) => v,
            other => panic!("trip() did not return an int: {other:?}"),
        };
        for _ in 0..WARM_CALLS {
            let _ = vm.invoke(CLASS, "warmStatic", "(I)I", &[Value::Int(ITERS_PER_WARM)]);
        }
        let delta_static = match vm.invoke(CLASS, "tripStatic", "()I", &[]) {
            Ok(Some(Value::Int(v))) => v,
            other => panic!("tripStatic() did not return an int: {other:?}"),
        };
        println!("spliced lambda: delta={delta} delta_static={delta_static}");
        assert_eq!(
            delta, 1,
            "the spliced lambda body's side effect ran {delta} times for ONE call: a trap \
             inside the splice re-ran the impl's committed `iastore`"
        );
        assert_eq!(delta_static, 1, "the static control");
    });
}

fn body() {
    let mut vm = Vm::new(VmConfig::new().with_classpath(vec![test_resources_dir()]));

    for _ in 0..WARM_CALLS {
        let _ = vm.invoke(CLASS, "warm", "(I)I", &[Value::Int(ITERS_PER_WARM)]);
    }
    wait_until_optimizing_compiled(&mut vm);

    // LAMBDA ARM FIRST, deliberately. The order is load-bearing: with the
    // control's 12 800 extra warm calls in front of it this reported delta=1,
    // and without them it reported delta=2 once. Whichever way that resolves,
    // the arm under test must not be measured from a state the control warmed.
    let delta = match vm.invoke(CLASS, "trip", "()I", &[]) {
        Ok(Some(Value::Int(v))) => v,
        other => panic!("trip() did not return an int: {other:?}"),
    };

    // The NON-LAMBDA control: a compiled caller invoking a compiled callee that
    // traps, with no SAM anywhere. `DeoptRerunCount` already covers the
    // one-frame case (interpreted caller, compiled callee) and reports 1; if
    // this two-frame control also reports 2, the duplication is about the extra
    // COMPILED frame and not about lambdas.
    for _ in 0..WARM_CALLS {
        let _ = vm.invoke(CLASS, "warmStatic", "(I)I", &[Value::Int(ITERS_PER_WARM)]);
    }
    let delta_static = match vm.invoke(CLASS, "tripStatic", "()I", &[]) {
        Ok(Some(Value::Int(v))) => v,
        other => panic!("tripStatic() did not return an int: {other:?}"),
    };

    let (resumed, unresumable) = cratonvm_vm::runtime::interpreter::lambda_site_deopt_outcomes();
    // The dispatch split (`calls` / `direct` / `no_code` / `refused`) would say
    // WHICH stage of the door did not happen, and during the investigation a
    // temporary accessor reported `calls=0` — the door is never entered, not
    // merely declining. It is not exposed here: `no_test_only_public_api` is a
    // slack-free ratchet and its message is explicit that a `pub` item in
    // `vm/src` whose only caller is a test does not get one. The number is
    // recorded on the known-issue page instead.
    println!(
        "lambda site: resumed={resumed} unresumable={unresumable} | \
         delta={delta} delta_static={delta_static}"
    );

    // ENGAGEMENT FIRST. The sibling file asserts `unresumable == 0` from a
    // fixture with no SAM in it, where both counters are structurally zero.
    // This is the check that makes the zero below mean something.
    assert!(
        resumed > 0,
        "no lambda-site deopt was resumed in this run (resumed={resumed} \
         unresumable={unresumable}), so the `unresumable == 0` below would be a \
         statement about an untouched counter — exactly the vacuity this file \
         exists to remove. Measured 2026-09-08 with a temporary accessor: the \
         SAM door's `calls` counter is 0 here, so the door is never ENTERED — \
         not entered and declining — which is what the known-issue page records"
    );
    assert_eq!(
        unresumable, 0,
        "a lambda-site deopt could not be resumed, so the generic path re-ran the impl from \
         entry, side effects and all"
    );
    assert_eq!(
        delta, 1,
        "the compiled lambda body's side effect ran {delta} times for ONE call. 2 means the \
         one-shot door answered the deopt by re-entering the impl from bci 0, re-running the \
         `iastore` it had already committed — and note the counters can be clean while this is \
         wrong, which is why the observable is asserted too"
    );
}
