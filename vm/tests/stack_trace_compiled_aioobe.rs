// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! An `ArrayIndexOutOfBoundsException` raised BY compiled code must still name
//! the frames that raised it.
//!
//! The third member of a family. `stack_trace_compiled_callee.rs` pins the
//! implicit-NPE half (fixed 2026-09-02);
//! `pgo02_guarded_virtual_inline.rs::check_stack_trace_through_an_inlined_frame`
//! pins the div-by-zero half (fixed 2026-09-05, after failing ~20% of runs for
//! as long as anyone had looked at it). This file is the third implicit signal.
//!
//! # Why it exists, and what it found
//!
//! `ImplicitSignal::Aioobe` has neither half of the machinery the other two
//! needed: no `snapshot_trap_frames` call at any of its eight setters, and no
//! `attach_snapshotted_trap_frames` at any of the six doors that construct the
//! throwable. Reading that, the natural conclusion — and the one written into
//! the div-by-zero record's §7 — was that an AIOOBE from compiled code must
//! carry an empty trace too.
//!
//! **It does not.** This file was written to fail first and did not, on any of
//! the three shapes below, including the one structurally identical to the
//! div-by-zero reproduction. Whatever route the bounds check takes here leaves
//! the interpreter holding the frames, so the trace `fillInStackTrace` builds is
//! already complete and there is nothing for a snapshot to add.
//!
//! That makes this a GUARD rather than a regression test: it pins a property
//! that currently holds by construction and that the other two signals had to
//! have restored. If a future change routes the bounds check through the
//! epilogue the way `jit_throw_arithmetic` does, this goes red, and the fix is
//! the five-part recipe in
//! `jit-arithmetic-exception-from-compiled-code-has-an-empty-trace-FIXED-20260905.md`.
//!
//! # The interpreter is the oracle
//!
//! Nothing here compares against a literal frame list. The SAME call on the
//! SAME entry point, made before it has compiled, is the expected answer — so
//! the fixture can be edited without editing this file, and a change that broke
//! both paths equally still fails the non-empty floor.
//!
//! # One entry point per measurement
//!
//! Each shape has its own, and they are driven in one process on one `Vm`: the
//! tiered background compile worker is process-global and does not warm up
//! again for a later `Vm` (the rung `pgo02_guarded_virtual_inline` documents),
//! while a cold reading needs an entry point nothing else has warmed (the
//! defect that file was found to have).

use cratonvm_types::error::MethodCallFailed;
use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

const CLASS: &str = "cratonvm/CompiledAioobeTrace";

/// Past the default `CRATONVM_JIT_THRESHOLD` (500), for the same reason
/// `pgo02_guarded_virtual_inline` uses 700: what matters is that a REAL compile
/// happened, not that some iteration count was reached.
const CALLS: i32 = 700;

/// Out of bounds for the fixture's length-4 arrays.
const BAD_INDEX: i32 = 9;

/// One per shape. Each names the route its bounds check takes.
const SHAPES: &[(&str, &str)] = &[
    (
        "directAtForTrace",
        "an `iaload` in the entry point's own compiled body",
    ),
    (
        "callAtForTrace",
        "an `iaload` behind a virtual call on a static-final receiver — the shape that lost \
         its frames for ArithmeticException",
    ),
    (
        "storeAtForTrace",
        "an `iastore`, which reaches the check through the void-return store helpers",
    ),
];

fn test_resources_dir() -> String {
    if let Some(generated) = option_env!("CRATONVM_TEST_CLASSES_DIR") {
        if std::path::Path::new(generated).is_dir() {
            return generated.to_owned();
        }
    }
    format!("{}/tests/resources", env!("CARGO_MANIFEST_DIR"))
}

/// The fixture is checked in beside its `.java`, so a box without `javac` runs
/// this too. If it is ever un-staged, say so rather than skipping: a green run
/// that asserted nothing is the failure mode a trace test is least able to
/// notice about itself.
fn fixture_staged() -> bool {
    std::path::Path::new(&format!(
        "{}/cratonvm/CompiledAioobeTrace.class",
        test_resources_dir()
    ))
    .exists()
}

fn test_vm() -> Vm {
    Vm::new(VmConfig::new().with_classpath(vec![test_resources_dir()]))
}

fn invoke_int(vm: &mut Vm, method: &str, args: &[Value]) -> i32 {
    match vm.invoke(CLASS, method, "(I)I", args) {
        Ok(Some(Value::Int(n))) => n,
        other => panic!("{method} failed or returned a non-int: {other:?}"),
    }
}

/// `Class.method` for every frame of the trace the VM captured for the
/// `ArrayIndexOutOfBoundsException` that `method(BAD_INDEX)` raises.
///
/// Reads the CAPTURED `Vec<StackTraceEntry>` rather than reconstructing one
/// through Java reflection, for the same reason its siblings do: these in-tree
/// tests boot the embedding default (`JdkMode::Synthetic`), whose `Throwable`
/// has no `getStackTrace()`. The captured vector is what `getStackTrace()` is
/// served from, so this reads the structure the consumer reads.
fn aioobe_frames(vm: &mut Vm, method: &str) -> Result<Vec<String>, String> {
    let err = match vm.invoke(CLASS, method, "(I)I", &[Value::Int(BAD_INDEX)]) {
        Err(e) => e,
        Ok(v) => {
            return Err(format!(
                "{method}({BAD_INDEX}) returned {v:?} instead of raising — the fixture is not \
                 indexing out of bounds, so this check would be vacuous"
            ))
        }
    };
    let MethodCallFailed::ExceptionThrown(exc) = err else {
        return Err(format!(
            "{method}({BAD_INDEX}) failed without a throwable: {err:?}"
        ));
    };
    let hash = vm.shared.mem.heap.identity_hash_code(exc);
    let trace = vm
        .shared
        .throwable_stack_trace(hash)
        .ok_or_else(|| format!("no stack trace was captured for {method}'s throwable"))?;
    Ok(trace
        .iter()
        .map(|f| format!("{}.{}", f.class_name, f.method_name))
        .collect())
}

/// Wait for `method`'s artifact, calling it meanwhile.
///
/// Compilation is ASYNCHRONOUS: crossing the invocation threshold enqueues the
/// method and a background worker installs the artifact some time later.
/// Reading the cache once and concluding "never compiled" is a race, not a
/// result — the rung `pgo02_guarded_virtual_inline::compiled_tally` documents.
/// The bound is generous because a loaded host is when the worker is slowest;
/// exceeding it is still a failure, because "it compiled" is the premise of
/// every assertion here.
fn wait_until_compiled(vm: &mut Vm, method: &str) -> Result<(), String> {
    const ATTEMPTS: usize = 400;
    const CALLS_PER_ATTEMPT: i32 = 50;
    for _ in 0..ATTEMPTS {
        let class_id = vm
            .shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(CLASS)
            .ok_or_else(|| "the fixture class must be loaded after invoke".to_string())?;
        if vm
            .shared
            .jit
            .jit_cache
            .read()
            .get(CLASS, method, "(I)I", class_id)
            .is_some()
        {
            return Ok(());
        }
        for i in 0..CALLS_PER_ATTEMPT {
            let _ = invoke_int(vm, method, &[Value::Int(i % 4)]);
        }
    }
    Err(format!(
        "{method} never compiled after {} calls — every assertion in this file is about \
         COMPILED code, so there would be nothing here to measure",
        ATTEMPTS as i32 * CALLS_PER_ATTEMPT + CALLS
    ))
}

#[test]
fn an_aioobe_from_compiled_code_names_the_frames_that_raised_it() {
    assert!(
        fixture_staged(),
        "vm/tests/resources/cratonvm/CompiledAioobeTrace.class is not staged. It is checked \
         in, so this means it was deleted or the resources directory moved — fix that rather \
         than skipping, because a skip here is a green run that asserted nothing."
    );

    // ONE `Vm` for every shape: the tiered background compile worker is
    // process-global and does not warm up again for a later one.
    let mut vm = test_vm();

    for &(method, route) in SHAPES {
        let interpreted = aioobe_frames(&mut vm, method).unwrap_or_else(|e| panic!("{e}"));
        assert!(
            !interpreted.is_empty(),
            "{method}: the INTERPRETED trace is empty, so comparing the compiled one against \
             it would prove nothing. The fixture or the capture path changed."
        );
        assert!(
            interpreted.iter().any(|f| f.starts_with(CLASS)),
            "{method}: the INTERPRETED trace names no fixture frame: {interpreted:?}"
        );

        for i in 0..CALLS {
            let _ = invoke_int(&mut vm, method, &[Value::Int(i % 4)]);
        }
        wait_until_compiled(&mut vm, method).unwrap_or_else(|e| panic!("{e}"));

        let compiled = aioobe_frames(&mut vm, method).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            compiled, interpreted,
            "{method} ({route}): the captured stack trace of an \
             `ArrayIndexOutOfBoundsException` raised in COMPILED code names {compiled:?}; the \
             interpreted path names {interpreted:?}.\n\
             \n\
             An EMPTY compiled trace is the shape this file guards against: the bounds check \
             would then be flagging its signal and leaving the compiled frames before the \
             throwable is built, which is what `jit_throw_arithmetic` does and what needed \
             five separate fixes there. The recipe is in \
             `jit-arithmetic-exception-from-compiled-code-has-an-empty-trace-FIXED-20260905.md`."
        );
    }
}
