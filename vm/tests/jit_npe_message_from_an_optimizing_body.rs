// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A null array store inside an OPTIMIZING-tier body must carry HotSpot's JEP
//! 358 message, like the same store raised by the interpreter or by a
//! single-pass body.
//!
//! # The defect this pins
//!
//! `jit_npe_message_from_compiled_code.rs` failed about one run in fifteen on
//! `storeToNull`, with the gate ON and `getMessage() == null`. Which body ran
//! the null call was a race:
//!
//! * usually the SINGLE-PASS body, whose null check deopts at bci 21. The
//!   interpreter replays the `iastore` and raises it with the full message;
//! * sometimes the OPTIMIZING body, once the C2 supersede had published. That
//!   body stores through the void helper `jit_iastore`, which flags the
//!   pending NPE and RETURNS, and the body returns normally with it.
//!
//! The second path is drained by `execute`'s normal-return arm in
//! `vm/src/runtime/interpreter.rs`. That was the one pending-NPE drain the
//! JEP 358 wirings of 2026-09-06 and 2026-09-11 missed: it built
//! `NullPointerException { message: None }` and left the recorded action code
//! unconsumed. The four `jit_bridge.rs` drains and the implicit-signal
//! materializer all go through `jit_npe_message`, and now it does too.
//!
//! # Why a second file instead of a fix to the first
//!
//! The first file cannot choose its tier timing, which is why it only failed
//! sometimes. Measured on the unfixed drain: this file's tier settings make
//! the optimizing body SUPERSEDE the single-pass one a few hundred calls after
//! it, and the sibling then failed 20 of 20 runs (0 of 20 with the fix, every
//! one of them through this drain).
//!
//! The settings are REAL environment variables on a re-executed child, not
//! `flags::with_process_overrides`. The overrides were tried first and did not
//! reproduce the environment's schedule: in-process, `lengthOfNull` sometimes
//! never compiled and `storeToNull` usually never got its optimizing body, so
//! the file failed before it measured anything. The child takes exactly the
//! path the measurement above took.
//!
//! The door matters, not the code. The same 700-byte optimizing body,
//! published FIRST (no single-pass body before it, as
//! `jit_deopt_sink_resumes_a_side_effecting_trap.rs` arranges), reaches the
//! null store through `execute`'s first-call tier-up sink, which deopts and
//! replays; that file's overrides therefore never reach this drain.
//!
//! # How this could pass while asserting nothing, and what closes it
//!
//! * **Never reaching the drain.** The child runs with `CRATONVM_DBG_JITNPE=1`,
//!   and the parent requires its trace of a message rebuilt for this store.
//!   Only a pending-NPE drain asks `jit_npe_message` for one. The deopt path,
//!   where the interpreter replays the `iastore`, never does. A child that
//!   missed the path is retried, up to five times, because on a loaded host
//!   about one in ten does. A child that reached it and got the wrong message
//!   fails at once.
//! * **A broken message reader.** The interpreted CONTROL asserts a sibling's
//!   text before anything is compiled, so a `None` below is a fact about the
//!   VM and not about the reader.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::error::MethodCallFailed;
use cratonvm_vm::memory::ArrayElementType;
use cratonvm_vm::types::{ObjectRef, Value};
use cratonvm_vm::vm::Vm;

const CLASS: &str = "cratonvm/CompiledNpeMessage";
const METHOD: &str = "storeToNull";
const DESC: &str = "(I)I";

/// Verbatim `java` output for this fixture (see the sibling file's oracle).
const HOTSPOT_MESSAGE: &str =
    "Cannot store to int array because \"cratonvm.CompiledNpeMessage.NULL_ARRAY\" is null";

/// The sibling file's warm-up, call for call. Past the default single-pass
/// threshold (500); what carries the count past `CHILD_ENV`'s C2 threshold is
/// `wait_until_compiled`'s calls while the single-pass compile is still in
/// flight.
const WARM_CALLS: i32 = 700;

/// The sibling file's shapes, warmed in the sibling file's order. The order is
/// part of the witness: it is the sequence that, under `CHILD_ENV`, took the
/// normal-return drain 20 runs out of 20.
const SHAPES: [&str; 3] = ["lengthOfNull", "loadFromNull", "storeToNull"];

/// Single-pass at the default threshold (500), then the optimizing tier a few
/// hundred calls later, so the optimizing body arrives as a SUPERSEDE of a
/// live single-pass body: the order that reaches the normal-return drain.
/// Set on the re-executed child; see the module header for why not in-process.
/// `CRATONVM_DBG_JITNPE` makes `jit_npe_message` say what it rebuilt, which is
/// how the parent checks that the drain was reached.
const CHILD_ENV: &[(&str, &str)] = &[
    ("CRATONVM_TIER_C2_THRESHOLD", "800"),
    ("CRATONVM_TIER_C2_MIN_INVOCATIONS", "700"),
    ("CRATONVM_DBG_JITNPE", "1"),
];

/// What `jit_npe_message` prints when it rebuilds this store's message from
/// the trapping bytecode, which only a pending-NPE drain asks it to do.
const REBUILT_TRACE: &str = "[JITNPE] rebuilt from bytecode: Cannot store to int array";

/// Marks the re-executed child, which runs [`body`] instead of spawning.
const CHILD_MARKER: &str = "CRATONVM_TEST_NPE_OPTIMIZING_BODY_CHILD";

const TEST_NAME: &str = "a_null_store_raised_by_an_optimizing_body_carries_hotspots_message";

fn test_resources_dir() -> String {
    if let Some(generated) = option_env!("CRATONVM_TEST_CLASSES_DIR") {
        if std::path::Path::new(generated).is_dir() {
            return generated.to_owned();
        }
    }
    format!("{}/tests/resources", env!("CARGO_MANIFEST_DIR"))
}

/// FAIL rather than skip when the fixture is missing: a green run that
/// asserted nothing is the failure mode this file exists to prevent.
fn assert_fixture_staged() {
    let path = format!("{}/cratonvm/CompiledNpeMessage.class", test_resources_dir());
    assert!(
        std::path::Path::new(&path).exists(),
        "fixture not staged at {path} — rebuild with `javac -d vm/tests/resources \
         vm/tests/resources/cratonvm/CompiledNpeMessage.java` and commit the .class"
    );
}

fn read_test_java_string(vm: &Vm, obj: ObjectRef) -> Option<String> {
    let heap = &vm.shared.mem.heap;
    let value_array = match heap.get_field(obj, 0) {
        Value::Object(Some(arr)) => arr,
        _ => return None,
    };
    match heap.array_element_type(value_array) {
        Some(ArrayElementType::Byte) => {
            let coder = match heap.get_field(obj, 1) {
                Value::Int(v) => v,
                _ => 0,
            };
            let bytes: Vec<u8> = (0..heap.array_length(value_array))
                .filter_map(|i| match heap.get_array_element(value_array, i).ok()? {
                    Value::Int(v) => Some(v as u8),
                    _ => None,
                })
                .collect();
            if coder == 1 {
                let units: Vec<u16> = bytes
                    .chunks_exact(2)
                    .map(|p| u16::from_le_bytes([p[0], p[1]]))
                    .collect();
                Some(String::from_utf16_lossy(&units))
            } else {
                Some(bytes.into_iter().map(char::from).collect())
            }
        }
        Some(ArrayElementType::Char) => {
            let units: Vec<u16> = (0..heap.array_length(value_array))
                .filter_map(|i| match heap.get_array_element(value_array, i).ok()? {
                    Value::Int(v) => Some(v as u16),
                    _ => None,
                })
                .collect();
            Some(String::from_utf16_lossy(&units))
        }
        _ => None,
    }
}

/// The throwable's message, located by DECODING rather than by field name, for
/// the reason the sibling file gives: the name-based lookup reads `None` for
/// the synthetic-JDK throwable these tests boot.
fn throwable_detail_message(vm: &Vm, exc: ObjectRef) -> Option<String> {
    let class_id = vm.shared.mem.heap.class_id_of(exc);
    if let Some(m) = vm
        .instance_field_index(class_id, "detailMessage")
        .and_then(|idx| match vm.shared.mem.heap.get_field(exc, idx) {
            Value::Object(Some(m)) => read_test_java_string(vm, m),
            _ => None,
        })
    {
        return Some(m);
    }
    let n = vm.shared.mem.heap.num_fields(exc);
    (0..n).find_map(|i| match vm.shared.mem.heap.get_field(exc, i) {
        Value::Object(Some(o)) => read_test_java_string(vm, o),
        _ => None,
    })
}

fn compiled(vm: &Vm, method: &str) -> Option<bool> {
    let class_id = vm
        .shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id(CLASS)
        .expect("the fixture class must be loaded after invoke");
    vm.shared
        .jit
        .jit_cache
        .read()
        .get(CLASS, method, DESC, class_id)
        .map(|c| c.used_ir_backend)
}

/// Wait for ANY artifact of `method`, calling its returning path meanwhile:
/// the sibling file's `wait_until_compiled`, unchanged.
fn wait_until_compiled(vm: &mut Vm, method: &str) {
    for _ in 0..400 {
        if compiled(vm, method).is_some() {
            return;
        }
        for _ in 0..50 {
            let _ = vm.invoke(CLASS, method, DESC, &[Value::Int(1)]);
        }
    }
    panic!("{CLASS}.{method} never compiled");
}

/// Trip `method`'s null path and return its NPE's message.
fn npe_message(vm: &mut Vm, method: &str) -> Option<String> {
    let err = match vm.invoke(CLASS, method, DESC, &[Value::Int(0)]) {
        Err(e) => e,
        Ok(v) => panic!(
            "{method}(0) returned {v:?} instead of raising — the fixture is not storing through \
             null, so this check would be vacuous"
        ),
    };
    let MethodCallFailed::ExceptionThrown(exc) = err else {
        panic!("{method}(0) failed without a throwable: {err:?}");
    };
    throwable_detail_message(vm, exc)
}

#[test]
fn a_null_store_raised_by_an_optimizing_body_carries_hotspots_message() {
    assert_fixture_staged();
    if std::env::var_os(CHILD_MARKER).is_some() {
        body();
        return;
    }
    // The schedule is a race with the background compiler, and on a loaded
    // host about one child in ten does not reach the drain: a warm-up that
    // never compiled, or a null call that went down the deopt path. Such a
    // run measured NOTHING, so it is retried. A run that reached the store and
    // got the wrong message is never retried: that is the defect.
    const ATTEMPTS: usize = 5;
    let mut vacuous = Vec::new();
    for _ in 0..ATTEMPTS {
        match run_child() {
            ChildRun::Measured => return,
            ChildRun::Vacuous(why) => vacuous.push(why),
        }
    }
    panic!(
        "{ATTEMPTS} children in a row never reached the pending-NPE drain, so this file measured \
         nothing:\n{}",
        vacuous.join("\n---\n")
    );
}

enum ChildRun {
    /// The store's NPE went through a pending-NPE drain and every assertion held.
    Measured,
    /// The child never reached the path under test; carries why.
    Vacuous(String),
}

/// Run [`body`] in a re-executed child under [`CHILD_ENV`]. Panics on any
/// failure that is a finding rather than a missed schedule.
fn run_child() -> ChildRun {
    let exe = std::env::current_exe().expect("the test binary's own path");
    let out = std::process::Command::new(exe)
        .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
        .env(CHILD_MARKER, "1")
        .envs(CHILD_ENV.iter().copied())
        .output()
        .expect("re-executing the test binary");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        // `wait_until_compiled`'s panic: the warm-up never compiled, which is
        // a missed schedule and not a finding.
        if stderr.contains("never compiled") || stdout.contains("never compiled") {
            return ChildRun::Vacuous(format!("warm-up never compiled:\n{stderr}"));
        }
        panic!(
            "the child run failed ({}):\n--- stdout\n{stdout}\n--- stderr\n{stderr}",
            out.status
        );
    }
    // Anti-vacuity for the harness itself: the child must have RUN the test,
    // not filtered it out.
    assert!(
        stdout.contains("1 passed"),
        "the child did not run {TEST_NAME}:\n{stdout}"
    );
    // Anti-vacuity for the PATH: the NPE must have come through a pending-NPE
    // drain, not the deopt-and-replay path the interpreter serves itself. A
    // pass without the trace is a pass that measured nothing.
    if !stderr.contains(REBUILT_TRACE) {
        return ChildRun::Vacuous(format!(
            "passed without `{REBUILT_TRACE}` on stderr, i.e. through the deopt path:\n{stderr}"
        ));
    }
    ChildRun::Measured
}

fn body() {
    let mut vm = Vm::new(VmConfig::new().with_classpath(vec![test_resources_dir()]));

    // CONTROL: an interpreted NPE's message, read by the same reader. It is
    // taken on a SIBLING method on purpose: running `storeToNull`'s own null
    // arm before the warm-up gives that branch a taken profile, and the
    // optimizing tier then never supersedes the single-pass body at all.
    cratonvm_vm::runtime::env_cache::set_show_code_details_in_exception_messages(true);
    assert_eq!(
        npe_message(&mut vm, "lengthOfNull").as_deref(),
        Some(
            "Cannot read the array length because \"cratonvm.CompiledNpeMessage.NULL_ARRAY\" is null"
        ),
        "interpreted control: the reader could not see a message the VM did write"
    );

    for m in SHAPES {
        for _ in 0..WARM_CALLS {
            let _ = vm.invoke(CLASS, m, DESC, &[Value::Int(1)]);
        }
        wait_until_compiled(&mut vm, m);
    }

    // Gate OFF first: unmessaged, as HotSpot is.
    cratonvm_vm::runtime::env_cache::set_show_code_details_in_exception_messages(false);
    assert_eq!(
        npe_message(&mut vm, METHOD),
        None,
        "with the gate off the NPE must stay unmessaged"
    );

    // Gate ON, twice, so that whatever the first call does to the installed
    // body (a deopt, an invalidation) is measured as well.
    cratonvm_vm::runtime::env_cache::set_show_code_details_in_exception_messages(true);
    for call in 0..2 {
        assert_eq!(
            npe_message(&mut vm, METHOD).as_deref(),
            Some(HOTSPOT_MESSAGE),
            "call {call}: a null `iastore` raised from an OPTIMIZING body must carry the message \
             the interpreter gives it. `None` means the NPE went through `execute`'s \
             normal-return drain in `interpreter.rs` without `jit_npe_message`"
        );
    }

    cratonvm_vm::runtime::env_cache::set_show_code_details_in_exception_messages(false);
}
