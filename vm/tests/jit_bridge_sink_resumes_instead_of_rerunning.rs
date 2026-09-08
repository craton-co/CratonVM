// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A trap in a compiled body reached through ORDINARY BYTECODE DISPATCH must
//! resume that body, not re-enter it from bci 0 and run its side effects twice.
//!
//! # The defect this pins
//!
//! A deopt sentinel does not mean "nothing happened". It means the compiled
//! body ran up to the trapping bci and stopped. `resume_deopted_body`'s own doc
//! says so in as many words — *"This is the only correct answer for a body that
//! has already committed a side effect"* — and cites the hibernate-reactive
//! `reactiveRemove`-fires-twice defect that came of a sink dropping the frame.
//!
//! Three sinks in `jit_bridge.rs` nevertheless answered it by re-running:
//! `execute_jit_call` (`jit-callsite-a`, the `invokestatic` path),
//! `execute_jit_call_decoded` (`jit-callsite-b`, the virtual/interface path)
//! and `resume_deopted_body` (both one-shot lambda doors). Each tried a precise
//! resume behind two preconditions that are false on every optimizing-tier
//! artifact in a production build —
//!
//! * `ir_deopt_resume_enabled()` (`CRATONVM_IR_DEOPT_RESUME`) is default OFF,
//!   and its own comment justifies that with *"no production IR method emits a
//!   deopt guard yet"*, which stopped being true;
//! * `compiled.can_deopt_resume` is set by `ir_lower` only under
//!   `CRATONVM_SCALAR_DEOPT` + `CRATONVM_DEOPT_REAL`
//!
//! — and then fell through to `Ok(None)` / `CacheMiss`, which means "re-execute
//! this method from entry". No side-effect check guards that fallback, unlike
//! the tier-up sink's, so the duplication is silent.
//!
//! # The measurement
//!
//! `DeoptRerunCount.hot` stores into an array (an `iastore`, which is
//! `opcode_commits_side_effect`) and then divides (an `idiv`, which the IR tier
//! lowers to a *deopt guard*, not a throw). `trip()` reads the counter either
//! side of one trapping call and returns the delta:
//!
//! | delta | meaning |
//! |---|---|
//! | **1** | the frame was resumed at the trapping bci — the store ran once |
//! | **2** | the method was re-entered from bci 0 — the store ran TWICE |
//!
//! The exception is not the subject. `trip()` catches the `ArithmeticException`
//! the re-executed `idiv` raises; what this file asserts is the COUNT.
//!
//! # Why EVERYTHING here goes through Java, warm-up included
//!
//! `vm.invoke` enters `execute`, and that matters twice over.
//!
//! For the trip: `execute`'s own tier-up sink was fixed on 2026-09-07 and
//! resumes correctly, so calling `hot` from Rust would measure the wrong sink.
//! `trip()` is Java, so its `invokestatic hot` is a real dispatch through
//! `dispatch_static` into `execute_jit_call` -- the sink under test.
//!
//! For the warm-up the reason is different, and was found the hard way. With
//! `CRATONVM_JIT_C2_FIRST_CALL` off (the default), `execute`'s first-call path
//! compiles with the SINGLE-PASS backend directly and caches a non-IR body,
//! "which preempts the optimizing IR pipeline on every later path (dispatcher
//! warmup, OSR, background worker all probe `jit_cache` first)" -- its own
//! words. Warming `hot` through `vm.invoke` therefore pinned it at C1 forever,
//! where `can_deopt_resume` IS set and the defect is invisible. The
//! anti-vacuity check below caught exactly that on the first run, which is the
//! whole reason it checks the BACKEND and not merely that an artifact exists.
//! Warming through `warm()` keeps every call to `hot` on the
//! invocation-counted dispatch path, which is the one that reaches C2.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

const CLASS: &str = "cratonvm/DeoptRerunCount";
const HOT: &str = "hot";
const HOT_DESC: &str = "(II)I";

/// `warm(ITERS_PER_WARM)` per outer call; the PRODUCT is what has to clear
/// `TIER_OVERRIDES`' C2 threshold, since each inner iteration is one dispatch
/// of `hot`.
const WARM_CALLS: i32 = 200;
const ITERS_PER_WARM: i32 = 64;

/// Drive `hot` straight from the interpreter to C2.
///
/// At C1 the single-pass backend sets `can_deopt_resume` itself, so the sink's
/// EXISTING precise-resume arm fires and the defect is invisible. Only the
/// optimizing IR backend leaves the flag false on a production artifact.
/// Raising the C1 threshold above the C2 one takes the `Interpreter -> C2` door
/// rather than waiting 20 000 invocations for a C1 method to be superseded.
///
/// Process-scoped, not thread-scoped: the compile runs on a background worker.
const TIER_OVERRIDES: &[(&str, Option<&str>)] = &[
    ("CRATONVM_TIER_C1_THRESHOLD", Some("100000")),
    ("CRATONVM_TIER_C2_THRESHOLD", Some("600")),
    ("CRATONVM_TIER_C2_MIN_INVOCATIONS", Some("500")),
    // ...and KEEP the optimizing body once it is built.
    //
    // The tier rows above get `hot` compiled at C2; this row is what stops the
    // acceptance gate throwing that body away again. Measured without it:
    //
    //     [ir] admission RerunDrv.hot(II)I: admitted to the optimizing pipeline
    //     [ir] acceptance RerunDrv.hot(II)I: REFUSED (evidence: none)
    //                                        -- keeping the single-pass body
    //
    // and the single-pass body sets `can_deopt_resume`, so the sink's existing
    // arm resumes and the defect is invisible. That is not the gate being
    // wrong: a method simple enough to be a clean witness for ONE store and ONE
    // trap is, by construction, too simple for an optimizing body to earn its
    // keep on evidence. This file is about what the sinks do with an
    // optimizing-tier artifact, not about which methods deserve one.
    ("CRATONVM_C2_ACCEPT", Some("always")),
];

fn test_resources_dir() -> String {
    if let Some(generated) = option_env!("CRATONVM_TEST_CLASSES_DIR") {
        if std::path::Path::new(generated).is_dir() {
            return generated.to_owned();
        }
    }
    format!("{}/tests/resources", env!("CARGO_MANIFEST_DIR"))
}

fn assert_fixture_staged() {
    let path = format!("{}/cratonvm/DeoptRerunCount.class", test_resources_dir());
    assert!(
        std::path::Path::new(&path).exists(),
        "fixture not staged at {path} — rebuild with `javac -d vm/tests/resources \
         vm/tests/resources/cratonvm/DeoptRerunCount.java` and commit the .class"
    );
}

/// Wait for `hot`'s OPTIMIZING artifact, driving its non-trapping path from
/// BYTECODE (see the module header on why not through `vm.invoke`).
///
/// The BACKEND is checked, not merely that an artifact exists: a single-pass
/// body sets `can_deopt_resume` and the sink's pre-existing arm resumes it, so
/// a green run against one would assert nothing about this defect.
fn wait_until_optimizing_compiled(vm: &mut Vm) {
    const ATTEMPTS: usize = 400;
    const CALLS_PER_ATTEMPT: i32 = 50;
    let mut saw_any = false;
    for _ in 0..ATTEMPTS {
        let class_id = vm
            .shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(CLASS)
            .expect("the fixture class must be loaded after invoke");
        if let Some(compiled) = vm
            .shared
            .jit
            .jit_cache
            .read()
            .get(CLASS, HOT, HOT_DESC, class_id)
        {
            saw_any = true;
            if compiled.used_ir_backend {
                return;
            }
        }
        for _ in 0..CALLS_PER_ATTEMPT {
            let _ = vm.invoke(CLASS, "warm", "(I)I", &[Value::Int(ITERS_PER_WARM)]);
        }
    }
    panic!(
        "{CLASS}.{HOT} never reached the OPTIMIZING backend (an artifact was {}) — this file is \
         about a trap taken inside an IR-compiled body, so proceeding would measure the \
         single-pass backend and pass for the wrong reason",
        if saw_any {
            "installed, but from the single-pass door"
        } else {
            "never installed"
        },
    );
}

#[test]
fn a_trap_reached_through_bytecode_dispatch_runs_its_side_effect_once() {
    assert_fixture_staged();
    cratonvm_types::flags::with_process_overrides(TIER_OVERRIDES, body);
}

fn body() {
    let mut vm = Vm::new(VmConfig::new().with_classpath(vec![test_resources_dir()]));

    for _ in 0..WARM_CALLS {
        let _ = vm.invoke(CLASS, "warm", "(I)I", &[Value::Int(ITERS_PER_WARM)]);
    }
    wait_until_optimizing_compiled(&mut vm);

    // Anti-vacuity: the warm-up itself must not have tripped anything, or the
    // delta below would be measuring a counter that is already moving.
    let _ = vm.invoke(CLASS, "reset", "()V", &[]);
    // Same for the residual census: warm-up traps are not this call's business.
    cratonvm_vm::runtime::interpreter::reset_deopt_frame_bail_counts();

    let delta = match vm.invoke(CLASS, "trip", "()I", &[]) {
        Ok(Some(Value::Int(n))) => n,
        other => panic!(
            "trip() did not return an int: {other:?} — it catches the ArithmeticException \
             itself, so an escaping error means the trap took a path this file does not model"
        ),
    };

    // The residual, asserted to be absent rather than assumed.
    //
    // `delta == 1` says THIS call resumed. It does not say the frame was
    // rebuilt rather than the trap simply not happening, and it says nothing
    // about any other trap the same call took. `deopt_frame_bail_total` is
    // every way `build_deopt_frame_inner` can decline — and a decline is
    // exactly what still falls back to re-running the method from entry, side
    // effects and all. Zero here is what makes the residual of
    // `jit-bridge-sinks-re-ran-a-side-effecting-body-FIXED-20260907.md`
    // measured rather than merely described.
    let bails = cratonvm_vm::runtime::interpreter::deopt_frame_bail_counts();
    let bail_total: u64 = bails.iter().map(|(_, n)| *n).sum();
    assert_eq!(
        bail_total,
        0,
        "a trapped frame could not be rebuilt, so its method re-ran FROM ENTRY          with its side effects: {}",
        bails
            .iter()
            .filter(|(_, n)| *n > 0)
            .map(|(why, n)| format!("{why}={n}"))
            .collect::<Vec<_>>()
            .join(" "),
    );

    // `lambda_site_deopt_outcomes()`'s own doc calls `unresumable` "the metric a
    // regression test asserts is zero", and until now no test asserted it —
    // the accessor had no caller anywhere in the tree. This is that caller.
    //
    // BUT READ THE NEXT PARAGRAPH BEFORE TREATING IT AS COVERAGE (2026-09-08).
    // This zero is STRUCTURAL. `DeoptRerunCount` contains no SAM anywhere — it
    // is `invokestatic` throughout — so neither `SITE_RESUMED` nor
    // `SITE_UNRESUMABLE` can be incremented by this fixture, and the assertion
    // below would hold just as firmly on a VM whose lambda door was completely
    // broken. Measured: `lambda_site_dispatch_counts()` reports `calls=0` for
    // this file, so the door is not merely declining, it is never entered.
    //
    // It is kept as a belt — a non-zero here would still be a real failure —
    // and the earned version lives in `jit_lambda_door_deopt_resumes.rs`, which
    // drives a real SAM and asserts ENGAGEMENT (`resumed > 0`) beside the zero.
    // That file is `#[ignore]`d because it reproduces an open defect: a lambda
    // callee's deopt is orphaned by an identity check comparing the SAM's name,
    // and its side effect runs twice
    // (`lambda-callee-deopt-is-orphaned-by-the-sam-name-check-20260908`).
    let (_resumed, unresumable) = cratonvm_vm::runtime::interpreter::lambda_site_deopt_outcomes();
    assert_eq!(
        unresumable, 0,
        "a lambda-site deopt could not be resumed, so the generic path re-ran          the impl from entry, side effects and all"
    );

    assert_eq!(
        delta, 1,
        "the compiled body's side effect ran {delta} times for ONE call. 2 means the sink \
         answered the deopt by re-entering the method from bci 0, re-running the `iastore` the \
         compiled body had already committed — a silent duplicated side effect, which is the \
         defect this file pins. See `cratonvm_jit::deopt_sink_resume_enabled` and \
         `resume_deopted_body`'s own doc: a deopt sentinel means the body ran up to the \
         trapping bci and stopped, not that nothing happened"
    );
}
