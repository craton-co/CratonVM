// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 8 (lane W8REV), `j27-intrinsic-coverage-vs-hotspot-20261028.md`:
//! loop-shaped single-pass intrinsic sites the optimizing tier may take as an
//! ORDINARY CALL instead of refusing the whole enclosing method.
//!
//! The single-pass backend answers `Arrays.fill` (integral overloads),
//! `Arrays.sort` (integral, whole array) and `CRC32C.update` (`(I)V`,
//! `([BII)V`, SSE4.2 hosts) inline (`JitIntrinsic::ArraysFill*`,
//! `ArraysSort*`, `Crc32cUpdate*`). The optimizing planner has no node for
//! them, so before this a method holding one of these sites was refused from
//! the optimizing tier ("loop-shaped or memory call-site intrinsic"). Under
//! [`SWITCH`] the planner instead keeps the site as a call row, exactly the
//! shape it already gives a cold OSR intrinsic site and the default-ON
//! `Arrays.equals` admission (`ir_arrays_equals_call_admits`). The callee is
//! then the JDK body (or its registered native), so every answer and every
//! exception (`NullPointerException` on a null array) is the JDK's.
//!
//! Opt-in because it is a trade nobody has measured: the method gains the
//! optimizing tier, the site loses the inline `REP STOS` / sort / CRC32 loop to
//! a call. Compile time only, no static.

/// `CRATONVM_JIT_IR_LOOP_INTRINSIC_CALL` (token `ir-loop-intrinsic-call`,
/// default OFF, `=1` enables, since 2026-10-28).
pub(crate) const SWITCH: &str = "CRATONVM_JIT_IR_LOOP_INTRINSIC_CALL";

/// Is this triple one of the loop-shaped single-pass intrinsic rows the
/// optimizing tier keeps as a call (switch on)? The rows mirror
/// `try_resolve_intrinsic`'s `ARRAYS_OPS` (fill half), `ARRAYS_SORT` and
/// `CRC32` regions; the unit test pins that each one IS a single-pass row.
pub(crate) fn admits(class: &str, method: &str, descriptor: &str) -> bool {
    is_loop_intrinsic_row(class, method, descriptor)
        && cratonvm_types::flags::runtime_flag_on(SWITCH)
}

fn is_loop_intrinsic_row(class: &str, method: &str, descriptor: &str) -> bool {
    match (class, method) {
        ("java/util/Arrays", "fill") => matches!(
            descriptor,
            "([BB)V" | "([ZZ)V" | "([CC)V" | "([SS)V" | "([II)V" | "([JJ)V"
            | "([BIIB)V" | "([ZIIZ)V" | "([CIIC)V" | "([SIIS)V" | "([IIII)V" | "([JIIJ)V"
        ),
        ("java/util/Arrays", "sort") => {
            matches!(descriptor, "([I)V" | "([J)V" | "([C)V" | "([S)V" | "([B)V")
        }
        ("java/util/zip/CRC32C", "update") => matches!(descriptor, "(I)V" | "([BII)V"),
        _ => false,
    }
}

#[cfg(test)]
mod tests;
