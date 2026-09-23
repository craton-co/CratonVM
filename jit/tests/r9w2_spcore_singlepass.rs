// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 9, wave 2, `spcore` lane: executed fixtures for the
//! single-pass backend changes (control flow, FP binops). Each method is compiled by
//! `x64::compile` and RUN against a host oracle (same harness as
//! `r9_baseline_control_and_stack.rs`).
//!
//! * `goto_w` is admitted by `jit_scan` and lowered by `walk_control`
//!   (`single-pass-refuses-every-method-containing-goto-w-20260918.md`).
//! * A fused `const; if_icmp*` BACK edge canonicalises a live operand stack,
//!   as every other branch arm does (NOTES-baseline cross-lane request 2).

#![cfg(target_arch = "x86_64")]

use cratonvm_jit::x64::{compile, jit_scan};
use cratonvm_jit::CompiledMethod;
use cratonvm_jit_api::JitRuntimeHelpers;
use std::collections::{HashMap, HashSet};

fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn record_throw_bci(_bci: i64) {}
    unsafe extern "C" fn deopt_unserviceable(_vm: i64, _info: i64, _args: i64, _n: i64) -> i64 {
        i64::MIN
    }
    JitRuntimeHelpers {
        set_throw_bci: record_throw_bci as *const () as usize,
        service_callee_deopt: deopt_unserviceable as *const () as usize,
        ..Default::default()
    }
}

/// Compile `code` (two padding bytes appended, as the in-crate fixtures do).
fn compile_method(code: &[u8], num_params: usize, max_locals: usize) -> Option<CompiledMethod> {
    let mut padded = code.to_vec();
    padded.extend_from_slice(&[0, 0]);
    compile(
        &padded,
        code.len(),
        num_params,
        max_locals,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &helpers(),
        HashSet::new(),
        HashMap::new(),
        None,
    )
}

fn call(m: &CompiledMethod, args: &[i64]) -> i64 {
    // SAFETY: every fixture below is a self-contained static int method that
    // reaches no runtime helper; the body was compiled by this test for
    // exactly these argument kinds.
    unsafe { m.try_call(args).expect("test JIT call") }
}

/// `f(n) = { s = 0; for (i = 0; i < n; i++) s += i; return s; }` with the loop's
/// back edge written as `goto_w`.
const GOTO_W_BACK_EDGE: [u8; 23] = [
    0x03, // 0: iconst_0
    0x3c, // 1: istore_1            ; i = 0
    0x03, // 2: iconst_0
    0x3d, // 3: istore_2            ; s = 0
    0x1b, // 4: iload_1             ; header
    0x1a, // 5: iload_0
    0xa2, 0x00, 0x0f, // 6: if_icmpge +15 -> 21
    0x1c, // 9: iload_2
    0x1b, // 10: iload_1
    0x60, // 11: iadd
    0x3d, // 12: istore_2
    0x84, 0x01, 0x01, // 13: iinc 1, 1
    0xc8, 0xff, 0xff, 0xff, 0xf4, // 16: goto_w -12 -> 4
    0x1c, // 21: iload_2
    0xac, // 22: ireturn
];

/// `f(x) = x != 0 ? 7 : 9`, the fall-through arm leaving its value on the
/// operand stack across a forward `goto_w` (a live merge at 15), with two
/// unreachable padding bytes after the `goto_w`.
const GOTO_W_FORWARD_WITH_LIVE_OPERAND: [u8; 16] = [
    0x1a, // 0: iload_0
    0x99, 0x00, 0x0c, // 1: ifeq +12 -> 13
    0x10, 0x07, // 4: bipush 7
    0xc8, 0x00, 0x00, 0x00, 0x09, // 6: goto_w +9 -> 15
    0x00, // 11: nop (unreachable)
    0x00, // 12: nop (unreachable)
    0x10, 0x09, // 13: bipush 9
    0xac, // 15: ireturn
];

#[test]
fn jit_scan_admits_goto_w() {
    assert!(
        jit_scan(&GOTO_W_BACK_EDGE, GOTO_W_BACK_EDGE.len(), "(I)I").is_some(),
        "a method with a goto_w back edge must pass the admission scan"
    );
    assert!(
        jit_scan(
            &GOTO_W_FORWARD_WITH_LIVE_OPERAND,
            GOTO_W_FORWARD_WITH_LIVE_OPERAND.len(),
            "(I)I"
        )
        .is_some(),
        "a method with a forward goto_w must pass the admission scan"
    );
    // A goto_w whose offset runs past the method is still refused.
    let truncated = [0x1a, 0xc8, 0x00, 0x00];
    assert!(jit_scan(&truncated, truncated.len(), "(I)I").is_none());
}

#[test]
fn a_goto_w_back_edge_runs_the_loop() {
    let m = compile_method(&GOTO_W_BACK_EDGE, 1, 3).expect("goto_w loop compiles");
    for n in [0i64, 1, 2, 5, 100, 1000] {
        assert_eq!(call(&m, &[n]), n * (n - 1) / 2, "n = {n}");
    }
    assert_eq!(call(&m, &[-3]), 0, "n < 0 never enters the loop");
}

#[test]
fn a_forward_goto_w_carries_its_live_operand_to_the_merge() {
    let m =
        compile_method(&GOTO_W_FORWARD_WITH_LIVE_OPERAND, 1, 1).expect("forward goto_w compiles");
    for x in [1i64, -1, 42, i64::from(i32::MAX), i64::from(i32::MIN)] {
        assert_eq!(call(&m, &[x]), 7, "x = {x}");
    }
    assert_eq!(call(&m, &[0]), 9);
}

/// `push n; do { n += 5; i++; } while (i < 10); return pushed - n` — the
/// loop's back edge is `bipush 10; if_icmplt`, which the const-compare
/// peephole fuses, with the pushed `n` live across the loop header. The
/// header is canonicalised when the walk reaches it, so the fused back edge
/// must canonicalise too; before, it jumped back with the operand in
/// whatever home the body left it.
#[test]
fn a_fused_const_compare_back_edge_keeps_a_live_operand() {
    let code = [
        0x1a, // 0: iload_0             ; the operand that outlives the loop
        0x03, // 1: iconst_0
        0x3c, // 2: istore_1            ; i = 0
        0x84, 0x00, 0x05, // 3: iinc 0, 5   ; header
        0x84, 0x01, 0x01, // 6: iinc 1, 1
        0x1b, // 9: iload_1
        0x10, 0x0a, // 10: bipush 10
        0xa1, 0xff, 0xf7, // 12: if_icmplt -9 -> 3
        0x1a, // 15: iload_0
        0x64, // 16: isub
        0xac, // 17: ireturn
    ];
    let m = compile_method(&code, 1, 2).expect("fused back-edge fixture compiles");
    for n in [0i64, 7, -100, 1 << 16] {
        assert_eq!(call(&m, &[n]), -50, "n = {n}");
    }
}

/// `f(x, y) = (int) (((a*b + a) - b/a) + a*a)` over `double a = x, b = y`.
/// The `ddiv` runs with the `a*b + a` result live below it (in XMM0 since the
/// FP-binop change), so the XMM0 flush and the XMM-resident operand paths
/// (`single-pass-fp-binops-round-trip-through-gpr-20260918.md`) are both on
/// the path.
#[test]
fn double_binops_chain_through_xmm_registers() {
    let code = [
        0x1a, // 0: iload_0
        0x87, // 1: i2d
        0x49, // 2: dstore_2            ; a
        0x1b, // 3: iload_1
        0x87, // 4: i2d
        0x39, 0x04, // 5: dstore 4      ; b
        0x28, // 7: dload_2
        0x18, 0x04, // 8: dload 4
        0x6b, // 10: dmul               ; a*b
        0x28, // 11: dload_2
        0x63, // 12: dadd               ; a*b + a
        0x18, 0x04, // 13: dload 4
        0x28, // 15: dload_2
        0x6f, // 16: ddiv               ; b/a
        0x67, // 17: dsub
        0x28, // 18: dload_2
        0x28, // 19: dload_2
        0x6b, // 20: dmul               ; a*a
        0x63, // 21: dadd
        0x8e, // 22: d2i
        0xac, // 23: ireturn
    ];
    let m = compile_method(&code, 2, 6).expect("double chain compiles");
    for (x, y) in [
        (3i32, 4i32),
        (7, -2),
        (-5, 11),
        (1, 0),
        (0, 5),
        (0, 0),
        (1000, 999),
        (-3, -3),
    ] {
        let (a, b) = (f64::from(x), f64::from(y));
        // Rust's `as i32` saturates and maps NaN to 0: JVMS `d2i`.
        let want = (((a * b + a) - b / a) + a * a) as i32;
        assert_eq!(
            call(&m, &[i64::from(x), i64::from(y)]) as i32,
            want,
            "x = {x}, y = {y}"
        );
    }
}

/// The `float` twin: `f(x, y) = (int) (((a*b + a) - b/a) + a*a)` over
/// `float a = x, b = y`.
#[test]
fn float_binops_chain_through_xmm_registers() {
    let code = [
        0x1a, // 0: iload_0
        0x86, // 1: i2f
        0x45, // 2: fstore_2            ; a
        0x1b, // 3: iload_1
        0x86, // 4: i2f
        0x46, // 5: fstore_3            ; b
        0x24, // 6: fload_2
        0x25, // 7: fload_3
        0x6a, // 8: fmul
        0x24, // 9: fload_2
        0x62, // 10: fadd
        0x25, // 11: fload_3
        0x24, // 12: fload_2
        0x6e, // 13: fdiv
        0x66, // 14: fsub
        0x24, // 15: fload_2
        0x24, // 16: fload_2
        0x6a, // 17: fmul
        0x62, // 18: fadd
        0x8b, // 19: f2i
        0xac, // 20: ireturn
    ];
    let m = compile_method(&code, 2, 4).expect("float chain compiles");
    for (x, y) in [
        (3i32, 4i32),
        (7, -2),
        (-5, 11),
        (1, 0),
        (0, 5),
        (0, 0),
        (1000, 999),
        (-3, -3),
    ] {
        let (a, b) = (x as f32, y as f32);
        let want = (((a * b + a) - b / a) + a * a) as i32;
        assert_eq!(
            call(&m, &[i64::from(x), i64::from(y)]) as i32,
            want,
            "x = {x}, y = {y}"
        );
    }
}

/// A ROTATED loop (`goto COND; BODY: ...; COND: if_icmplt BODY`) with a
/// loop-invariant `x*3 + 11` in its body — an integer-arithmetic LICM
/// candidate. Its header is bypassable (the entry `goto` lands in COND), so by
/// default the hoist is dropped; under `CRATONVM_JIT_ROTATED_PREHEADER=1` it is
/// emitted before the entry `goto`
/// (`rotated-loops-lose-every-preheader-transform-20260918.md`). Both must
/// compute `n * (3x + 11)` (wrapping), and the armed compile must differ.
const ROTATED_ARITH_LICM: [u8; 26] = [
    0x03, // 0: iconst_0
    0x3d, // 1: istore_2            ; s = 0
    0x03, // 2: iconst_0
    0x3e, // 3: istore_3            ; i = 0
    0xa7, 0x00, 0x0f, // 4: goto +15 -> 19
    0x1c, // 7: iload_2             ; header (BODY)
    0x1a, // 8: iload_0
    0x06, // 9: iconst_3
    0x68, // 10: imul
    0x10, 0x0b, // 11: bipush 11
    0x60, // 13: iadd
    0x60, // 14: iadd
    0x3d, // 15: istore_2
    0x84, 0x03, 0x01, // 16: iinc 3, 1
    0x1d, // 19: iload_3            ; COND
    0x1b, // 20: iload_1
    0xa1, 0xff, 0xf2, // 21: if_icmplt -14 -> 7
    0x1c, // 24: iload_2
    0xac, // 25: ireturn
];

#[test]
fn a_rotated_loop_keeps_its_arith_hoist_when_armed() {
    let compile_with = |armed: bool| {
        cratonvm_types::flags::with_thread_overrides(
            &[(
                "CRATONVM_JIT_ROTATED_PREHEADER",
                Some(if armed { "1" } else { "0" }),
            )],
            || compile_method(&ROTATED_ARITH_LICM, 2, 4),
        )
        .expect("rotated loop compiles")
    };
    let off = compile_with(false);
    let on = compile_with(true);
    for (x, n) in [
        (0i32, 0i32),
        (1, 1),
        (5, 10),
        (-7, 3),
        (i32::MAX, 4),
        (3, -2),
        (100, 1000),
    ] {
        let want = if n <= 0 {
            0
        } else {
            n.wrapping_mul(x.wrapping_mul(3).wrapping_add(11))
        };
        let args = [i64::from(x), i64::from(n)];
        assert_eq!(call(&off, &args) as i32, want, "default: x = {x}, n = {n}");
        assert_eq!(call(&on, &args) as i32, want, "armed: x = {x}, n = {n}");
    }
    assert_ne!(
        off.code_bytes(),
        on.code_bytes(),
        "arming the relocation must change the rotated loop's code (the hoist \
         is emitted before the entry goto and read in the body)"
    );
}

/// `lcmp; if<cond>` is fused into `CMP r64, r64; Jcc`. Every one of the six
/// conditions, over pairs that differ only in the high half, the sign, or
/// not at all: `f(x, y) = ((long) x <cond> (long) y) ? 1 : 0`.
#[test]
fn fused_lcmp_branches_take_the_signed_condition() {
    let conds: [(u8, fn(i64, i64) -> bool); 6] = [
        (0x99, |a, b| a == b), // ifeq
        (0x9a, |a, b| a != b), // ifne
        (0x9b, |a, b| a < b),  // iflt
        (0x9c, |a, b| a >= b), // ifge
        (0x9d, |a, b| a > b),  // ifgt
        (0x9e, |a, b| a <= b), // ifle
    ];
    let pairs = [
        (0i32, 0i32),
        (1, 2),
        (2, 1),
        (-1, 1),
        (1, -1),
        (i32::MIN, i32::MAX),
        (i32::MAX, i32::MIN),
        (-5, -5),
    ];
    for (op, holds) in conds {
        let code = [
            0x1a, // 0: iload_0
            0x85, // 1: i2l
            0x1b, // 2: iload_1
            0x85, // 3: i2l
            0x94, // 4: lcmp
            op, 0x00, 0x05, // 5: if<cond> +5 -> 10
            0x03, // 8: iconst_0
            0xac, // 9: ireturn
            0x04, // 10: iconst_1
            0xac, // 11: ireturn
        ];
        let m = compile_method(&code, 2, 2).expect("lcmp fixture compiles");
        for (x, y) in pairs {
            let want = i64::from(holds(i64::from(x), i64::from(y)));
            assert_eq!(
                call(&m, &[i64::from(x), i64::from(y)]),
                want,
                "op 0x{op:02x}: x = {x}, y = {y}"
            );
        }
    }
}

/// The fused pair as a loop's BACK edge: `long i = 0; do { i++; } while (i <
/// (long) n); return (int) i;`.
#[test]
fn a_fused_lcmp_back_edge_runs_the_loop() {
    let code = [
        0x09, // 0: lconst_0
        0x40, // 1: lstore_1           ; i (slots 1-2)
        0x1a, // 2: iload_0
        0x85, // 3: i2l
        0x42, // 4: lstore_3           ; n (slots 3-4)
        0x1f, // 5: lload_1            ; header
        0x0a, // 6: lconst_1
        0x61, // 7: ladd
        0x40, // 8: lstore_1
        0x1f, // 9: lload_1
        0x21, // 10: lload_3
        0x94, // 11: lcmp
        0x9b, 0xff, 0xf9, // 12: iflt -7 -> 5
        0x1f, // 15: lload_1
        0x88, // 16: l2i
        0xac, // 17: ireturn
    ];
    let m = compile_method(&code, 1, 5).expect("long do-while compiles");
    for n in [-3i64, 0, 1, 2, 17, 1000] {
        assert_eq!(call(&m, &[n]) as i32, n.max(1) as i32, "n = {n}");
    }
}

/// A value live BELOW the two `lcmp` operands must reach both arms of the
/// fused branch: `7 + ((long) x > (long) y ? 1 : 2)`.
#[test]
fn a_fused_lcmp_branch_keeps_the_operand_below_it() {
    let code = [
        0x10, 0x07, // 0: bipush 7
        0x1a, // 2: iload_0
        0x85, // 3: i2l
        0x1b, // 4: iload_1
        0x85, // 5: i2l
        0x94, // 6: lcmp
        0x9e, 0x00, 0x06, // 7: ifle +6 -> 13
        0x04, // 10: iconst_1
        0x60, // 11: iadd
        0xac, // 12: ireturn
        0x05, // 13: iconst_2
        0x60, // 14: iadd
        0xac, // 15: ireturn
    ];
    let m = compile_method(&code, 2, 2).expect("live-operand lcmp compiles");
    for (x, y) in [(3i64, 1i64), (1, 3), (5, 5), (-1, 0), (0, -1)] {
        let want = if x > y { 8 } else { 9 };
        assert_eq!(call(&m, &[x, y]), want, "x = {x}, y = {y}");
    }
}
