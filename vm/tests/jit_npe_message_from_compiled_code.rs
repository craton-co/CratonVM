// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A null array deref inside a COMPILED method must carry HotSpot's JEP 358
//! message when `ShowCodeDetailsInExceptionMessages` is on, and none when it is
//! off.
//!
//! # What this file found, which is not what it was written to find
//!
//! It was written to pin a fix. The JEP 358 apparatus for JIT-originated NPEs
//! looked wired to nothing: the compiled null-check stub records an action code
//! (`set_jit_pending_npe_action`), `DrainedJitSignals` carries it,
//! `helpful_npe::jit_action_message` maps it to a verbatim HotSpot string — and
//! all four pending-NPE drains in `jit_bridge.rs` built
//! `NullPointerException { message: None }` unconditionally, with
//! `jit_npe_message_gated` having no production caller at all.
//!
//! **The array shapes never reach those drains.** `jit_npe_with_action` records
//! the action and then sets a DEOPT: compiled code does not raise the NPE, it
//! bails to the interpreter, which replays the trapping bytecode and raises it
//! with the interpreter's own — FULLER — message, the one carrying the
//! `because "…" is null` clause. Instrumenting all four drains showed none of
//! them firing for any shape here.
//!
//! So the messages below are HotSpot-exact today and were before the drains
//! were wired. That wiring is a latent correctness fix — a drain that ever does
//! receive a non-`NONE` action would otherwise silently drop it — and it is not
//! what makes this test pass. This file does not pretend otherwise.
//!
//! # The oracle is real HotSpot
//!
//! The expected strings are what `java` prints for THIS fixture after 20 000
//! warm-up calls, i.e. from C2-compiled code:
//!
//! ```text
//! LEN=[Cannot read the array length because "cratonvm.CompiledNpeMessage.NULL_ARRAY" is null]
//! LOAD=[Cannot load from int array because "cratonvm.CompiledNpeMessage.NULL_ARRAY" is null]
//! ```
//!
//! # Two ways this file could pass while asserting nothing, both closed
//!
//! * **A broken message reader.** `instance_field_index(cid, "detailMessage")`
//!   — the idiom the real-JDK tests use — resolves to `None` for the
//!   synthetic-JDK `java/lang/NullPointerException` these in-tree tests boot,
//!   so it reads `None` for every throwable, messaged or not. It did exactly
//!   that here until the interpreted CONTROL below caught it. That control is
//!   the point: a reader that cannot see a message it is looking straight at is
//!   indistinguishable from a VM that never wrote one.
//! * **Never actually compiling.** `wait_until_compiled` fails rather than
//!   proceeding, because every claim here is about entering a compiled body.

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::error::MethodCallFailed;
use cratonvm_vm::memory::ArrayElementType;
use cratonvm_vm::types::{ObjectRef, Value};
use cratonvm_vm::vm::Vm;

const CLASS: &str = "cratonvm/CompiledNpeMessage";
const DESC: &str = "(I)I";

/// Past the default `CRATONVM_JIT_THRESHOLD` (500), for the reason the sibling
/// trace tests use 700: what matters is that a REAL compile happened.
const WARM_CALLS: i32 = 700;

/// `(method, HotSpot's message for it)` — verbatim `java` output for this
/// fixture, warmed into C2.
const SHAPES: &[(&str, &str)] = &[
    (
        "lengthOfNull",
        "Cannot read the array length because \
         \"cratonvm.CompiledNpeMessage.NULL_ARRAY\" is null",
    ),
    (
        "loadFromNull",
        "Cannot load from int array because \
         \"cratonvm.CompiledNpeMessage.NULL_ARRAY\" is null",
    ),
];

/// `storeToNull` is fixture-only and DELIBERATELY not asserted here.
///
/// HotSpot raises `NullPointerException: Cannot store to int array because
/// "cratonvm.CompiledNpeMessage.NULL_ARRAY" is null` for it. CratonVM raises no
/// NPE at all from compiled code: the `iastore` reaches the void-return store
/// helpers, which need a precise deopt to replay a side-effecting store, and
/// without one the call fails with `InternalError: precise deoptimization
/// unavailable for … at bci 21 (can_deopt_resume=false …); refusing
/// side-effecting replay`.
///
/// That is a real divergence, but a DIFFERENT one — upstream of any message,
/// since no NullPointerException is ever constructed — so asserting it here
/// would tie this file's fate to an unrelated bug. The fixture method stays so
/// the exclusion is visible rather than silently absent.
const UNASSERTED_STORE_SHAPE: &str = "storeToNull";

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

fn test_vm() -> Vm {
    Vm::new(VmConfig::new().with_classpath(vec![test_resources_dir()]))
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

/// The throwable's message, located by DECODING rather than by field name.
///
/// See the module header: the name-based lookup resolves to `None` for the
/// synthetic-JDK throwable, so it cannot tell "no message" from "cannot read
/// the message". Scanning reference slots for the first decodable Java `String`
/// is name- and layout-independent, and survives the
/// `backtrace`/`detailMessage`/`cause` slot ordering that
/// `cluster_c_constructor.rs` records.
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

/// Wait for `method`'s artifact, calling its RETURNING path meanwhile.
///
/// Compilation is asynchronous: crossing the threshold enqueues the method and
/// a background worker installs it later, so reading the cache once and
/// concluding "never compiled" is a race rather than a result.
fn wait_until_compiled(vm: &mut Vm, method: &str) {
    const ATTEMPTS: usize = 400;
    const CALLS_PER_ATTEMPT: i32 = 50;
    for _ in 0..ATTEMPTS {
        let class_id = vm
            .shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(CLASS)
            .expect("the fixture class must be loaded after invoke");
        if vm
            .shared
            .jit
            .jit_cache
            .read()
            .get(CLASS, method, DESC, class_id)
            .is_some()
        {
            return;
        }
        for _ in 0..CALLS_PER_ATTEMPT {
            let _ = vm.invoke(CLASS, method, DESC, &[Value::Int(1)]);
        }
    }
    panic!(
        "{CLASS}.{method} never compiled — every assertion here is about entering a COMPILED \
         body, so proceeding would measure the interpreter and pass for the wrong reason"
    );
}

/// Trip `method`'s null path and return its NPE's message.
fn npe_message(vm: &mut Vm, method: &str) -> Option<String> {
    let err = match vm.invoke(CLASS, method, DESC, &[Value::Int(0)]) {
        Err(e) => e,
        Ok(v) => panic!(
            "{method}(0) returned {v:?} instead of raising — the fixture is not dereferencing \
             null, so this check would be vacuous"
        ),
    };
    let MethodCallFailed::ExceptionThrown(exc) = err else {
        panic!("{method}(0) failed without a throwable: {err:?}");
    };
    throwable_detail_message(vm, exc)
}

#[test]
fn a_compiled_null_array_deref_carries_hotspots_message_only_when_the_gate_is_on() {
    assert_fixture_staged();
    let mut vm = test_vm();

    // CONTROL, before anything is warmed. The same deref raised by the
    // INTERPRETER with the gate on must carry the full text. This proves the
    // message plumbing AND this file's reader work, so a `None` from the
    // compiled arms below is a fact about the VM rather than about the
    // measurement — which is exactly the mistake this control caught once.
    cratonvm_vm::runtime::env_cache::set_show_code_details_in_exception_messages(true);
    assert_eq!(
        npe_message(&mut vm, SHAPES[0].0).as_deref(),
        Some(SHAPES[0].1),
        "interpreted control: the reader could not see a message the VM did write, so every \
         other assertion in this file would have been vacuous"
    );

    // The gate is read at RAISE time, not compile time, so one warm-up serves
    // both arms and they differ in nothing else.
    for (m, _) in SHAPES {
        for _ in 0..WARM_CALLS {
            let _ = vm.invoke(CLASS, m, DESC, &[Value::Int(1)]);
        }
        wait_until_compiled(&mut vm, m);
    }

    // --- gate OFF: unmessaged, the default-path shape.
    cratonvm_vm::runtime::env_cache::set_show_code_details_in_exception_messages(false);
    for (m, _) in SHAPES {
        assert_eq!(
            npe_message(&mut vm, m),
            None,
            "{m}: with the gate off the NPE must stay unmessaged — the default path must not \
             change when the JEP 358 text is not asked for"
        );
    }

    // --- gate ON: HotSpot's text, verbatim.
    cratonvm_vm::runtime::env_cache::set_show_code_details_in_exception_messages(true);
    for (m, want) in SHAPES {
        assert_eq!(
            npe_message(&mut vm, m).as_deref(),
            Some(*want),
            "{m}: a null array deref entered through a COMPILED body must produce HotSpot's \
             message. The compiled null check deopts and the interpreter replays the trapping \
             bytecode, so the `because …` clause is expected here; an action-only string would \
             mean the raise moved to one of `jit_bridge.rs`'s pending-NPE drains, which cannot \
             reconstruct the null expression"
        );
    }

    let _ = UNASSERTED_STORE_SHAPE;
    // Leave the process gate where an embedder expects it.
    cratonvm_vm::runtime::env_cache::set_show_code_details_in_exception_messages(false);
}
