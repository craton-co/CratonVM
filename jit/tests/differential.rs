// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! AUDIT-2026-05-16: Differential FP testing for the JIT.
//!
//! The roadmap calls out "JIT crashes on FP math" as a top blocker, yet the
//! existing FP unit tests in `x64.rs` cover only single-instruction load /
//! op / return patterns. This module exercises a longer chain — a counted
//! loop with floating-point accumulation — and compares the JIT's output
//! against the host's IEEE-754 evaluation of the same arithmetic.
//!
//! The host reference is what the interpreter computes too: both follow
//! standard f64/f32 semantics. Any divergence here is a JIT bug.
//!
//! These tests intentionally avoid `frem`/`drem` (currently unsupported —
//! see `test_jit_scan_rejects_frem_drem`) and stick to add/sub/mul/div
//! and conversions.

use cratonvm_jit::x64::{compile, is_jit_compatible};
use cratonvm_jit_api::JitRuntimeHelpers;
use std::collections::{HashMap, HashSet};

/// Dummy runtime helpers — none of the FP tests invoke heap allocation,
/// fields, type checks, or method dispatch, so the stub pointer is unused.
fn dummy_helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("JIT differential test invoked an unwired runtime helper");
    }
    let s = stub as *const () as usize;
    // `set_throw_bci` only records the throwing bci in a thread-local; the
    // backend calls it on the throw path of EVERY method that has an exception
    // check, so reaching it is normal rather than a sign of missing wiring.
    // Give it a real no-op instead of the panicking stub.
    unsafe extern "C" fn record_throw_bci(_bci: i64) {}
    let throw_bci = record_throw_bci as *const () as usize;
    unsafe extern "C" fn deopt_unserviceable_stub(
        _vm: i64,
        _info: i64,
        _args: i64,
        _n: i64,
    ) -> i64 {
        i64::MIN
    }
    let deopt_unserviceable = deopt_unserviceable_stub as *const () as usize;
    JitRuntimeHelpers {
        safepoint_flag_addr: 0,
        safepoint_slow_path: 0,
        jit_card_table_addr: 0,
        jit_card_old_base: 0,
        jit_card_old_end: 0,
        newarray: s,
        new_object: s,
        anewarray_object: s,
        baload: s,
        bastore: s,
        iaload: s,
        iastore: s,
        aaload: s,
        aastore: s,
        multianewarray_2d: s,
        arraylength: s,
        getfield: s,
        putfield_int: s,
        putfield_long: s,
        putfield_float: s,
        putfield_double: s,
        putfield_object: s,
        getstatic: s,
        putstatic_int: s,
        putstatic_long: s,
        putstatic_float: s,
        putstatic_double: s,
        putstatic_object: s,
        checkcast: s,
        instanceof_check: s,
        throw_aioobe: s,
        throw_arithmetic: s,
        invoke_dispatch: s,
        invoke_virtual_mic: s,
        lambda_int_to_double: s,
        write_barrier: s,
        satb_pre_write_barrier: s,
        uncommon_trap: s,
        math_fma_double: s,
        math_fma_float: s,
        // Inline TLAB bump wiring — see jit-api/src/lib.rs. Tests use the
        // helper-call fallback so leave the cursor offset at 0 (Tlab layout),
        // the end offset at 8, and `get_current_thread = 0` to opt out.
        tlab_cursor_offset_in_thread: 0,
        tlab_end_offset_in_thread: 8,
        class_id_offset_in_obj: 0,
        get_current_thread: 0,
        tlab_post_init: 0,
        frame_record: 0,
        shadow_stack_offset_in_thread: 0,
        throw_exception: s,
        jit_npe_with_action: s,
        dispatch_threw: s,
        jit_frem: s,
        jit_drem: s,
        self_call_stack_guard: 0,
        region_bounds_addr: 0,
        native_stack_floor_fn: 0,
        ldc_string: s,
        // Reached through emit_call_absolute, so a 0 here is a null CALL
        // (SIGSEGV), not an inert "unwired" sentinel. Neither is a sign of
        // missing wiring: `set_throw_bci` just records the throwing bci, and
        // `service_callee_deopt` is the normal IR direct-call path when a
        // callee returns the i64::MIN "threw" sentinel. The deopt stub returns
        // that sentinel unchanged -- exactly what the real helper does for a
        // vm/info it cannot service -- so the caller propagates the throw.
        set_throw_bci: throw_bci,
        service_callee_deopt: deopt_unserviceable,
        ..Default::default()
    }
}

/// Compile a self-contained method that takes no params and returns a value.
/// Returns the raw i64 the JIT call produced.
unsafe fn jit_run_no_args(code: &[u8], descriptor: &str, num_locals: usize) -> i64 {
    let code_len = code.len();
    assert!(
        is_jit_compatible(code, code_len, descriptor),
        "bytecode rejected by jit_scan — test setup is wrong"
    );
    let compiled = compile(
        code,
        code_len,
        0, // num_params
        num_locals,
        false, // needs_heap
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &dummy_helpers(),
        HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("JIT compilation failed");
    // SAFETY: `compiled` was produced by the JIT from valid bytecode and the
    // mmap region is executable.
    compiled.try_call(&[]).expect("test JIT call")
}

/// JVM bytecode for the method:
///   double sum_doubles_0_to_n_minus_1(int n) {
///       double s = 0.0;
///       for (int i = 0; i < n; i++) {
///           s = s + (double)i;
///       }
///       return s;
///   }
///
/// `n` is hard-wired to 10 via bipush so the test takes no params.
/// Local 0/1 hold the double `s`, local 2 holds the int `i`.
fn double_sum_loop_n10() -> Vec<u8> {
    vec![
        // 0:  dconst_0          ; push 0.0
        0x0e,
        // 1:  dstore_0          ; s = 0.0  (locals 0+1)
        // FIX: was 0x48 (dstore_1) — a typo that stored the accumulator into
        // local 1 while every dload_0 (0x26) read local 0, so `s` never
        // updated and the JIT (correctly) returned the initial 0.0. dstore_0
        // is 0x47.
        0x47, // 2:  iconst_0          ; push 0
        0x03, // 3:  istore_2          ; i = 0
        0x3d, // Loop head (PC 4):
        // 4:  iload_2           ; push i
        0x1c, // 5:  bipush 10         ; push 10
        0x10, 0x0a, // 7:  if_icmpge +14 → exit at PC 21
        0xa2, 0x00, 0x0e, // 10: dload_0           ; push s
        0x26, // 11: iload_2           ; push i
        0x1c, // 12: i2d               ; (double)i
        0x87, // 13: dadd              ; s + (double)i
        0x63,
        // 14: dstore_0          ; s = ...
        0x47, // FIX: was 0x48 (dstore_1) — see note at PC 1.
        // 15: iinc 2 1          ; i++
        0x84, 0x02, 0x01, // 18: goto -14 → PC 4
        0xa7, 0xff, 0xf2, // 21: dload_0           ; return s
        0x26, // 22: dreturn
        0xaf,
    ]
}

/// Diagnostic: stripped-down version that executes the loop body once
/// manually, without the loop. Returns s after one iteration with i=0:
/// expected 0.0 (since s starts at 0.0 and we add 0.0).
fn double_loop_body_once() -> Vec<u8> {
    vec![
        0x0e, // dconst_0
        0x47, // dstore_0   FIX: was 0x48 (dstore_1) — accumulator must use local 0.
        0x03, // iconst_0
        0x3d, // istore_2
        // Body
        0x26, // dload_0
        0x1c, // iload_2
        0x87, // i2d
        0x63, // dadd
        0x47, // dstore_0   FIX: was 0x48 (dstore_1).
        // Return
        0x26, // dload_0
        0xaf, // dreturn
    ]
}

#[test]
fn diagnostic_loop_body_once() {
    let code = double_loop_body_once();
    let raw = unsafe { jit_run_no_args(&code, "()D", 3) };
    let actual = f64::from_bits(raw as u64);
    println!(
        "diagnostic: body-once returned {actual} (bits: {:016x})",
        raw as u64
    );
    assert_eq!(actual, 0.0);
}

/// Diagnostic: i=5; return (double)i + 0.0 to verify i2d works.
fn double_i2d_simple() -> Vec<u8> {
    vec![
        0x08, // iconst_5
        0x3d, // istore_2
        0x1c, // iload_2
        0x87, // i2d
        0xaf, // dreturn
    ]
}

#[test]
fn diagnostic_i2d() {
    let code = double_i2d_simple();
    let raw = unsafe { jit_run_no_args(&code, "()D", 3) };
    let actual = f64::from_bits(raw as u64);
    println!(
        "diagnostic: i2d returned {actual} (bits: {:016x})",
        raw as u64
    );
    assert_eq!(actual, 5.0);
}

/// Diagnostic: int-only sum loop 0..10 = 45.
/// Locals 0=s, 1=i.
fn int_sum_loop_n10() -> Vec<u8> {
    vec![
        0x03, // 0: iconst_0
        0x3b, // 1: istore_0  (s=0)
        0x03, // 2: iconst_0
        0x3c, // 3: istore_1  (i=0)
        // Loop head (PC 4):
        0x1b, // 4: iload_1
        0x10, 0x0a, // 5: bipush 10
        0xa2, 0x00, 0x0d, // 7: if_icmpge +13 → 20
        0x1a, // 10: iload_0  (s)
        0x1b, // 11: iload_1  (i)
        0x60, // 12: iadd
        0x3b, // 13: istore_0
        0x84, 0x01, 0x01, // 14: iinc 1, 1
        0xa7, 0xff, 0xf3, // 17: goto -13 → 4
        0x1a, // 20: iload_0
        0xac, // 21: ireturn
    ]
}

#[test]
fn diagnostic_int_sum_loop() {
    let code = int_sum_loop_n10();
    let raw = unsafe { jit_run_no_args(&code, "()I", 2) };
    println!("diagnostic: int sum loop returned {raw}");
    assert_eq!(raw, 45);
}

#[test]
fn differential_double_sum_loop_matches_host() {
    // Host reference computation — same arithmetic the interpreter would do.
    let mut expected = 0.0f64;
    for i in 0..10 {
        expected += i as f64;
    }

    let code = double_sum_loop_n10();
    // SAFETY: the JIT-emitted code is loaded into executable memory by the
    // jit crate; we only invoke it after a successful compile.
    let raw = unsafe { jit_run_no_args(&code, "()D", 3) };
    let actual = f64::from_bits(raw as u64);

    assert_eq!(
        actual.to_bits(),
        expected.to_bits(),
        "JIT diverges from host for sum-of-doubles loop: \
         expected {expected} ({:016x}), got {actual} ({:016x})",
        expected.to_bits(),
        raw as u64,
    );
}

/// Same loop but with multiplication accumulator, to exercise dmul.
fn double_product_loop_n6() -> Vec<u8> {
    vec![
        // 0: dconst_1            ; push 1.0
        0x0f,
        // 1: dstore_0            ; s = 1.0
        0x47, // FIX: was 0x48 (dstore_1) — typo; dstore_0 is 0x47 (see sum-loop note).
        // 2: iconst_1            ; push 1
        0x04, // 3: istore_2            ; i = 1
        0x3d, // Loop head (PC 4):
        // 4: iload_2
        0x1c, // 5: bipush 7            ; bound: i < 7  (so i goes 1..6)
        0x10, 0x07, // 7: if_icmpge +14 → PC 21
        0xa2, 0x00, 0x0e, // 10: dload_0
        0x26, // 11: iload_2
        0x1c, // 12: i2d
        0x87, // 13: dmul
        0x6b, // 14: dstore_0
        0x47, // FIX: was 0x48 (dstore_1) — see PC 1.
        // 15: iinc 2 1
        0x84, 0x02, 0x01, // 18: goto -14 → PC 4
        0xa7, 0xff, 0xf2, // 21: dload_0
        0x26, // 22: dreturn
        0xaf,
    ]
}

#[test]
fn differential_double_product_loop_matches_host() {
    let mut expected = 1.0f64;
    for i in 1..7 {
        expected *= i as f64;
    }

    let code = double_product_loop_n6();
    let raw = unsafe { jit_run_no_args(&code, "()D", 3) };
    let actual = f64::from_bits(raw as u64);

    assert_eq!(
        actual.to_bits(),
        expected.to_bits(),
        "JIT diverges from host for product-of-doubles loop: \
         expected {expected}, got {actual}",
    );
}

/// Pure-FP arithmetic chain without a loop — exercises register allocator's
/// XMM colouring and conversion sequences. Computes
/// `((3.5 + 2.25) * 4.0 - 1.0) / 2.0` as doubles.
fn double_chain() -> Vec<u8> {
    // We use ldc2_w for constants, but ldc2_w needs the const pool. Easier
    // alternative: build constants via int → double conversions and stays
    // self-contained.
    //
    // Compute: s = ((3 + 2) * 4 - 1) / 2 as doubles → 9.5
    // Locals: 0=s (double, slots 0-1)
    vec![
        // iconst_3, i2d
        0x06, 0x87, // iconst_2, i2d
        0x05, 0x87, // dadd                 ; 5.0
        0x63, // iconst_4, i2d
        0x07, 0x87, // dmul                 ; 20.0
        0x6b, // iconst_1, i2d
        0x04, 0x87, // dsub                 ; 19.0
        0x67, // iconst_2, i2d
        0x05, 0x87, // ddiv                 ; 9.5
        0x6f, // dreturn
        0xaf,
    ]
}

#[test]
fn differential_double_chain_matches_host() {
    // Host reference exactly as the JIT will compute it.
    let expected: f64 = {
        let a = (3i32 as f64) + (2i32 as f64);
        let b = a * (4i32 as f64);
        let c = b - (1i32 as f64);
        c / (2i32 as f64)
    };

    let code = double_chain();
    let raw = unsafe { jit_run_no_args(&code, "()D", 2) };
    let actual = f64::from_bits(raw as u64);

    assert_eq!(
        actual.to_bits(),
        expected.to_bits(),
        "JIT diverges from host for double chain: expected {expected}, got {actual}",
    );
}

// --- BUG-JOIN-MIRROR-20260726 ---------------------------------------------
//
// The x64 single-pass backend elides a `MOV reg,[rbp-off]` reload when the
// immediately preceding instruction stored that very slot from that very
// register (the `slot_mirror`). At a branch target the mirror is cleared at
// the TOP of the PC's handling — but the merge-point `canonicalize_stack()`
// runs AFTER that clear, and the code it emits sits BEFORE `pc_to_native[pc]`,
// i.e. on the fall-through edge only. The mirror it recorded therefore leaked
// across the join, and the reload it suppressed left the joined value in a
// register only one of the two edges had written.
//
// `return s == null ? defaultValue : s` is the minimal shape: the
// fall-through arm canonicalizes `s` out of its callee-saved home via RAX
// (`MOV RAX,R12 ; MOV [rbp-off],RAX`) while the `goto` arm stores
// `defaultValue` straight from its own home (`MOV [rbp-off],R13`, no RAX).
// The `areturn`'s elided reload then returned the fall-through arm's RAX on
// BOTH edges — so the null case returned the null instead of the default.
// H2's `ConnectionInfo.getProperty(key, "rw")` answered null for every
// database open.

/// `static Object pick(Object s, Object defaultValue) {
///      return s == null ? defaultValue : s; }`
///
/// ```text
/// 0: aload_0                 2a
/// 1: ifnonnull 8             c7 00 07
/// 4: aload_1                 2b
/// 5: goto 9                  a7 00 04
/// 8: aload_0                 2a
/// 9: areturn                 b0
/// ```
fn ternary_pick() -> Vec<u8> {
    vec![
        0x2a, 0xc7, 0x00, 0x07, 0x2b, 0xa7, 0x00, 0x04, 0x2a, 0xb0, 0, 0,
    ]
}

#[test]
fn ternary_return_uses_the_value_from_the_taken_edge() {
    let code = ternary_pick();
    let compiled = compile(
        &code,
        code.len(),
        2, // num_params: (s, defaultValue)
        2, // max_locals
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
        &dummy_helpers(),
        HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("ternary pick must compile");
    // Two plausible non-null "object pointers" — the JIT only moves them.
    const S: i64 = 0x1234_5678;
    const DEF: i64 = 0x7EDC_BA98;
    // Warm both edges, alternating, exactly as a real caller would.
    for i in 0..8 {
        let taken_null = i % 2 == 0;
        let got = unsafe {
            compiled
                .try_call(&[if taken_null { 0 } else { S }, DEF])
                .expect("test JIT call")
        };
        let want = if taken_null { DEF } else { S };
        assert_eq!(got, want, "iteration {i}: null-edge={taken_null}");
    }
}
