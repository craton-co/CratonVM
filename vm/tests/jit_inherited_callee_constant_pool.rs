// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression: the callee compile gate must scan an inherited method's
//! bytecode against the constant pool of the class that DECLARES it.
//!
//! `try_jit_compile_callee_slow` resolves a callee with
//! `find_method_recursive`, which hands back the method *and* its declaring
//! class. It then scans that bytecode for forced `Class` generic-metadata
//! calls — a scan that resolves the constant-pool indices embedded in the
//! code. Those indices only mean anything in the DECLARING class's pool. The
//! gate was passing the RECEIVER's class instead, so for every inherited
//! method the same index was read out of an unrelated pool: a Utf8, a
//! Fieldref, or nothing at all. The scan's conservative "shape I don't
//! recognise ⇒ refuse" arm then fired and the method was PERMANENTLY
//! bail-listed.
//!
//! The blast radius was framework hierarchies whose hot methods are inherited
//! accessors. On `org.hibernate.orm.test.jpa.criteria.InPredicateTest` every
//! hot SQM accessor was bail-listed this way, so `CRATONVM_DBG=mic-prof`
//! reported `hit_entry=0` — compiled code's inline cache never once held a
//! callable entry — and JIT-on ran ~2x SLOWER than `--nojit`.
//!
//! The fixture (`resources/cratonvm/JitInheritedCalleeCp.java`) is built so
//! the two pools genuinely disagree: `Sub` declares nothing, so its pool is
//! short, while `Base.inheritedAccessor`'s `invokevirtual` sits past the end
//! of it. `constant_pools_actually_differ` asserts that precondition, so a
//! future fixture edit that made the pools accidentally compatible would show
//! up as a failing test rather than as a passing-but-vacuous one.

#![allow(clippy::unwrap_used)]

use cratonvm_vm::config::VmConfig;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::Vm;

const BASE: &str = "cratonvm/JitInheritedCalleeCp$Base";
const SUB: &str = "cratonvm/JitInheritedCalleeCp$Sub";
const ACCESSOR: (&str, &str) = ("inheritedAccessor", "()I");

/// `build.rs` stages freshly-compiled fixtures under `CRATONVM_TEST_CLASSES_DIR`
/// and does NOT write them back into the source tree, so a test that looks only
/// in `tests/resources` finds nothing for a NEW fixture and skips — reporting a
/// green "0 failed" for a test that never ran.
fn classpath_entries() -> Vec<String> {
    let mut entries = Vec::new();
    if let Some(staged) = option_env!("CRATONVM_TEST_CLASSES_DIR") {
        if std::path::Path::new(staged).exists() {
            entries.push(staged.to_string());
        }
    }
    entries.push(format!("{}/tests/resources", env!("CARGO_MANIFEST_DIR")));
    entries
}

fn class_files_available() -> bool {
    classpath_entries().iter().any(|entry| {
        std::path::Path::new(entry)
            .join("cratonvm/JitInheritedCalleeCp$Sub.class")
            .exists()
    })
}

macro_rules! require_class_files {
    () => {
        if !class_files_available() {
            eprintln!(
                "Skipping: JitInheritedCalleeCp$Sub.class not available (javac not on PATH?)"
            );
            return;
        }
    };
}

fn test_vm() -> Vm {
    let cfg = VmConfig::new().with_classpath(classpath_entries());
    Vm::new(cfg)
}

/// Drive the fixture so both classes are loaded and the accessor has run.
fn drive(vm: &mut Vm) {
    let r = vm.invoke(
        "cratonvm/JitInheritedCalleeCp",
        "drive",
        "(I)I",
        &[Value::Int(200)],
    );
    match r {
        // 200 iterations x "payload".length() == 7
        Ok(Some(Value::Int(1400))) => {}
        other => panic!("fixture did not run as expected: {other:?}"),
    }
}

/// The precondition this regression depends on: the index the declaring
/// class's `invokevirtual` carries must NOT resolve to a method reference in
/// the subclass's pool. If a fixture edit ever made the two pools compatible,
/// the regression test below would still pass while testing nothing — so
/// assert the disagreement directly.
#[test]
fn constant_pools_actually_differ() {
    require_class_files!();
    let mut vm = test_vm();
    drive(&mut vm);

    let cm = vm.shared.classes.class_manager.read();
    let base_id = cm
        .find_unique_class_by_name(BASE)
        .unwrap_or_else(|| panic!("{BASE} not loaded"));
    let sub_id = cm
        .find_unique_class_by_name(SUB)
        .unwrap_or_else(|| panic!("{SUB} not loaded"));
    let base = cm.get_class(base_id).expect("base class");
    let sub = cm.get_class(sub_id).expect("sub class");

    let method = base
        .find_method(ACCESSOR.0, ACCESSOR.1)
        .expect("Base declares inheritedAccessor");
    let code = &method.code().expect("accessor has code").code;

    // Find the accessor's `invokevirtual` (0xb6) operand — the index the gate
    // resolves. The body is `aload_0; getfield; invokevirtual; ireturn`, so a
    // direct scan for the opcode is unambiguous here.
    let mut invoke_index: Option<u16> = None;
    let mut pc = 0usize;
    while pc + 2 < code.len() {
        if code[pc] == 0xb6 {
            invoke_index = Some(u16::from_be_bytes([code[pc + 1], code[pc + 2]]));
            break;
        }
        pc += 1;
    }
    let invoke_index = invoke_index.expect("accessor body contains an invokevirtual");

    // In the DECLARING class's pool that index is a method reference...
    assert!(
        base.constant_pool.get(invoke_index).is_some(),
        "index {invoke_index} must exist in {BASE}'s pool — fixture is malformed"
    );
    // ...and in the SUBCLASS's pool it must not be one, or the gate's wrong
    // lookup would have accidentally succeeded and this fixture would prove
    // nothing.
    let sub_entry = sub.constant_pool.get(invoke_index);
    let sub_is_methodref = matches!(
        sub_entry,
        Some(cratonvm_reader::constant_pool::ConstantPoolEntry::MethodReference { .. })
            | Some(
                cratonvm_reader::constant_pool::ConstantPoolEntry::InterfaceMethodReference { .. }
            )
    );
    assert!(
        !sub_is_methodref,
        "fixture no longer exercises the bug: index {invoke_index} resolves to a \
         method reference in {SUB}'s pool too ({sub_entry:?})"
    );
}

/// The regression itself: asking the callee compile gate for the inherited
/// accessor **through the subclass** must not permanently bail-list it.
#[test]
fn inherited_callee_is_not_bail_listed_through_subclass() {
    require_class_files!();
    let mut vm = test_vm();
    drive(&mut vm);

    // Ask exactly what a compiled caller's entryless inline-cache miss asks:
    // resolve this callee from the RECEIVER's class name.
    let compiled = cratonvm_vm::runtime::interpreter::try_jit_compile_callee(
        &vm.shared, SUB, ACCESSOR.0, ACCESSOR.1, true,
    );

    assert!(
        !cratonvm_jit::is_jit_bail_listed(SUB, ACCESSOR.0, ACCESSOR.1),
        "inherited accessor was bail-listed when resolved through the subclass — \
         the gate is reading its bytecode against the wrong constant pool again"
    );
    assert!(
        compiled.is_some(),
        "the gate refused a plain inherited accessor; with the pools read \
         correctly it has nothing to refuse"
    );
}
