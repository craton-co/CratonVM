// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 4, lane SMALLFIX: the scalar-intrinsic switch is read per call
//! (IR2-5), and `StrictMath.abs(F/D)` lowers like `Math.abs(F/D)`.

use super::*;
use cratonvm_types::flags::with_thread_overrides;

const SCALAR: &str = "CRATONVM_JIT_IR_SCALAR_INTRINSICS";

fn scalar_row_with(value: Option<&str>, class: &str, method: &str, desc: &str) -> Option<ScalarOp> {
    with_thread_overrides(&[(SCALAR, value)], || try_ir_scalar_intrinsic(class, method, desc))
}

/// IR2-5: a `OnceLock` latch answered whatever the first reader saw for the
/// rest of the process, so a per-thread (or per-VM) `=0` could not reach it.
#[test]
fn a_thread_override_reaches_the_scalar_intrinsic_switch() {
    let row = ("java/lang/Math", "abs", "(I)I");
    assert_eq!(scalar_row_with(None, row.0, row.1, row.2), Some(ScalarOp::AbsI));
    assert_eq!(scalar_row_with(Some("0"), row.0, row.1, row.2), None);
    assert_eq!(scalar_row_with(None, row.0, row.1, row.2), Some(ScalarOp::AbsI));
}

/// The three neighbours converted with it answer a thread override too.
#[test]
fn the_converted_neighbour_switches_answer_a_thread_override() {
    let read = |key: &str, value: Option<&str>, f: fn() -> bool| {
        with_thread_overrides(&[(key, value)], f)
    };
    let guard = "CRATONVM_JIT_IR_TRAP_REPLAY_GUARD";
    assert!(read(guard, None, ir_trap_replay_guard_enabled));
    assert!(!read(guard, Some("0"), ir_trap_replay_guard_enabled));
    let spec = "CRATONVM_JIT_IR_SPECULATE";
    assert!(!read(spec, None, ir_speculate_enabled));
    assert!(read(spec, Some("1"), ir_speculate_enabled));
    let unresolved = "CRATONVM_JIT_IR_UNRESOLVED_CLASS_TRAP";
    assert!(!read(unresolved, None, ir_unresolved_class_trap_enabled));
    assert!(read(unresolved, Some("1"), ir_unresolved_class_trap_enabled));
}

/// JDK 25 `StrictMath.abs(float)` and `abs(double)` are `return Math.abs(a);`,
/// so the optimizing tier gives them the same sign-bit clear. The switch
/// still kills them, and the other widths' descriptors do not cross over.
#[test]
fn strictmath_abs_fp_lowers_as_math_abs() {
    let strict = "java/lang/StrictMath";
    for (desc, want) in [("(F)F", ScalarOp::AbsF), ("(D)D", ScalarOp::AbsD)] {
        let math = scalar_row_with(None, "java/lang/Math", "abs", desc);
        assert_eq!(math, Some(want), "Math.abs{desc}");
        let strict_on = scalar_row_with(None, strict, "abs", desc);
        assert_eq!(strict_on, Some(want), "StrictMath.abs{desc}");
        let strict_off = scalar_row_with(Some("0"), strict, "abs", desc);
        assert_eq!(strict_off, None, "=0 StrictMath.abs{desc}");
    }
    assert_eq!(scalar_row_with(None, strict, "abs", "(F)D"), None);
}
