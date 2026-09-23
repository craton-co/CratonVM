// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A trap taken in a COMPILED body whose method commits side effects must
//! resume that body's own frame — not raise
//! `InternalError: precise deoptimization unavailable … refusing
//! side-effecting replay`.
//!
//! # The defect this pins
//!
//! `execute`'s first-call tier-up sink (`vm/src/runtime/interpreter.rs`,
//! traced as `execute-first-call-tierup`) attempted a precise resume of a
//! stashed deopt frame only under `compiled.can_deopt_resume`, and raised a
//! hard, uncatchable `InternalError` when it could not. On an
//! **optimizing-tier** artifact that flag is false by construction —
//! `ir_lower` sets it only under `CRATONVM_SCALAR_DEOPT` +
//! `CRATONVM_DEOPT_REAL`, two non-default debug flags — so every trap taken in
//! an IR body reaching that sink, in a method that commits any side effect (any
//! store, any call), aborted.
//!
//! The frame it refused is the same frame
//! `vm::jit::helpers::try_resume_trapped_callee` — the sibling sink, on the
//! compiled caller's dispatch path — resumes PRECISELY, in production, with no
//! `can_deopt_resume` anywhere in its conditions. Both build it with the same
//! `build_deopt_frame_inner`. One sink could resume and the other declared the
//! same frame unusable; see `cratonvm_jit::deopt_sink_resume_enabled`.
//!
//! # The witness, and why it is this one
//!
//! `cratonvm/CompiledNpeMessage.storeToNull` — `NULL_ARRAY[0] = warm`, an
//! `iastore` through a null array reference. The IR tier lowers the store's
//! null check to a **deopt guard** rather than to a throw, on the promise that
//! the interpreter re-executes the opcode and raises the exception with full
//! semantics. Keeping that promise needs exactly the resume this file asserts.
//!
//! `iastore` is itself `opcode_commits_side_effect`, so
//! `replay_from_entry_is_observably_equivalent` — the sink's other escape hatch
//! — refuses this body, which is what left the abort as the only outcome.
//!
//! The fixture is shared with `jit_npe_message_from_compiled_code.rs`, whose
//! `UNASSERTED_STORE_SHAPE` constant recorded this exact abort as a known,
//! deliberately-unasserted divergence. That exclusion is what this file
//! retires: the shape now behaves, and behaving is now pinned.
//!
//! # What "behaving" means
//!
//! HotSpot raises `NullPointerException: Cannot store to int array because
//! "cratonvm.CompiledNpeMessage.NULL_ARRAY" is null`. The assertion here is on
//! the exception KIND and on the resume having happened at all — the message
//! text is the sibling file's subject, and asserting it twice would tie this
//! file's fate to the JEP 358 plumbing rather than to the deopt sink.
//!
//! # How this could pass while asserting nothing, and what closes it
//!
//! If `storeToNull` never compiled — or compiled only at the SINGLE-PASS tier,
//! which sets `can_deopt_resume` correctly and resumes on its own — the call
//! would raise a perfectly ordinary NPE and this file would pass having
//! measured something that was never broken.
//! `wait_until_optimizing_compiled` checks the BACKEND and panics rather than
//! proceeding, for that reason and no other.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::error::{MethodCallFailed, VmError};
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

const CLASS: &str = "cratonvm/CompiledNpeMessage";
const METHOD: &str = "storeToNull";
const DESC: &str = "(I)I";

/// Past `TIER_OVERRIDES`' C2 threshold, so the FIRST artifact this method gets
/// is the optimizing one.
const WARM_CALLS: i32 = 3_000;

/// Drive the method straight from the interpreter to C2.
///
/// Without this it lands at C1 — and the SINGLE-PASS backend sets
/// `can_deopt_resume` correctly (`x64/driver.rs`:
/// `!deopt_points.is_empty() && !has_elided_monitor`), so the sink resumes and
/// the defect is invisible. Only the optimizing IR backend leaves the flag
/// false on a production artifact, which is the whole subject here. Raising
/// the C1 threshold above the C2 one takes the `Interpreter -> C2` door in
/// `jit::tiered`'s `select_tier` rather than waiting 20 000 invocations for a
/// C1 method to be superseded.
///
/// Process-scoped and not thread-scoped: the compile runs on a BACKGROUND
/// worker, which a thread override would never reach.
const TIER_OVERRIDES: &[(&str, Option<&str>)] = &[
    ("CRATONVM_TIER_C1_THRESHOLD", Some("100000")),
    ("CRATONVM_TIER_C2_THRESHOLD", Some("600")),
    ("CRATONVM_TIER_C2_MIN_INVOCATIONS", Some("500")),
];

fn test_resources_dir() -> String {
    if let Some(generated) = option_env!("CRATONVM_TEST_CLASSES_DIR") {
        if std::path::Path::new(generated).is_dir() {
            return generated.to_owned();
        }
    }
    format!("{}/tests/resources", env!("CARGO_MANIFEST_DIR"))
}

/// The fixture is checked in beside its `.java`. If it is ever un-staged, FAIL
/// rather than skip: a green run that asserted nothing is precisely the failure
/// mode this file exists to prevent.
fn assert_fixture_staged() {
    let path = format!("{}/cratonvm/CompiledNpeMessage.class", test_resources_dir());
    assert!(
        std::path::Path::new(&path).exists(),
        "fixture not staged at {path} — rebuild with `javac -d vm/tests/resources \
         vm/tests/resources/cratonvm/CompiledNpeMessage.java` and commit the .class"
    );
}

/// Wait for `storeToNull`'s OPTIMIZING artifact, calling its RETURNING path
/// meanwhile.
///
/// Compilation is asynchronous: crossing the threshold enqueues the method and
/// a background worker installs it later, so reading the cache once and
/// concluding "never compiled" is a race rather than a result.
///
/// The BACKEND is checked, not merely that an artifact exists. The single-pass
/// body sets `can_deopt_resume` correctly and resumes on its own, so a green
/// run against one would assert nothing about this defect.
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
            .get(CLASS, METHOD, DESC, class_id)
        {
            saw_any = true;
            if compiled.used_ir_backend {
                return;
            }
        }
        for _ in 0..CALLS_PER_ATTEMPT {
            let _ = vm.invoke(CLASS, METHOD, DESC, &[Value::Int(1)]);
        }
    }
    panic!(
        "{CLASS}.{METHOD} never reached the OPTIMIZING backend (an artifact was {}) - every          assertion here is about a trap taken inside an IR-compiled body, so proceeding would          measure the single-pass backend, or the interpreter, and pass for the wrong reason",
        if saw_any {
            "installed, but from the single-pass door"
        } else {
            "never installed"
        },
    );
}

#[test]
fn a_side_effecting_body_that_traps_resumes_instead_of_aborting() {
    assert_fixture_staged();
    cratonvm_types::flags::with_process_overrides(TIER_OVERRIDES, body);
}

fn body() {
    let mut vm = Vm::new(VmConfig::new().with_classpath(vec![test_resources_dir()]));

    for _ in 0..WARM_CALLS {
        let _ = vm.invoke(CLASS, METHOD, DESC, &[Value::Int(1)]);
    }
    wait_until_optimizing_compiled(&mut vm);

    // Trip the null path. The compiled null check deopts, the sink rebuilds
    // this method's own frame from the stash and resumes it at the trapping
    // bci, and the interpreter re-executes the `iastore` — which throws.
    let err = match vm.invoke(CLASS, METHOD, DESC, &[Value::Int(0)]) {
        Err(e) => e,
        Ok(v) => panic!(
            "{METHOD}(0) returned {v:?} instead of raising — the fixture is not storing through \
             null, so this check would be vacuous"
        ),
    };

    match err {
        MethodCallFailed::ExceptionThrown(exc) => {
            // The KIND, not merely "something was thrown". A resume that parked
            // the frame at the wrong bci, or rebuilt the wrong locals, can
            // still raise — and an `ArrayIndexOutOfBoundsException` or a
            // `ClassCastException` here would be a resume that went wrong,
            // reported as a pass. The message text is the sibling file's
            // subject and is deliberately not asserted twice.
            let class_id = vm.shared.mem.heap.class_id_of(exc);
            let name = vm
                .shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .map(|c| c.name.to_string())
                .unwrap_or_else(|| format!("<unknown class {class_id:?}>"));
            assert_eq!(
                name, "java/lang/NullPointerException",
                "the resumed frame re-executed the `iastore` through a null array, so the                  interpreter must raise NullPointerException; anything else means the resume                  landed somewhere other than the trapping bci"
            );
        }
        MethodCallFailed::InternalError(VmError::Internal { ref message })
            if message.contains("precise deoptimization unavailable") =>
        {
            panic!(
                "the tier-up sink refused a frame it could rebuild and aborted instead of \
                 resuming: {message}\n\nThis is the 2026-09-07 cross-suite crash \
                 (`refusing side-effecting replay`). The sibling sink \
                 `try_resume_trapped_callee` resumes this same stash with the same builder; see \
                 `cratonvm_jit::deopt_sink_resume_enabled`."
            );
        }
        other => panic!(
            "{METHOD}(0) failed without a Java throwable, and not with the abort this file \
             pins either: {other:?}"
        ),
    }
}
