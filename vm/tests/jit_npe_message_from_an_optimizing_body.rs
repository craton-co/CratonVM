// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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



/// What `jit_npe_message` prints when it rebuilds this store's message from
/// the trapping bytecode, which only a pending-NPE drain asks it to do.
const REBUILT_TRACE: &str = "[JITNPE] rebuilt from bytecode: Cannot store to int array";



const TEST_NAME: &str = "a_null_store_raised_by_an_optimizing_body_carries_hotspots_message";





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



enum ChildRun {
    /// The store's NPE went through a pending-NPE drain and every assertion held.
    Measured,
    /// The child never reached the path under test; carries why.
    Vacuous(String),
}




