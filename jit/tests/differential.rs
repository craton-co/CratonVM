// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Differential testing for the JIT: the two backends against each other, and
//! both against a host-computed anchor.
//!
//! The file began as FP-only and single-backend (the AUDIT note below). The
//! second half — from the banner "Executing differential tests for the
//! semantics the JVMS pins and a JIT can silently get wrong" — is the part
//! that covers the opcodes where the two tiers can actually disagree, plus the
//! regression tests for this branch's two out-of-bounds P0s. Read this header
//! for the FP half's rationale and that banner for the rest.
//!
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

// ===========================================================================
// Executing differential tests for the semantics the JVMS pins and a JIT can
// silently get wrong.
//
// WHAT THIS FILE HAD, AND WHY THAT WAS THIN
//
// Everything above this line is floating point: four add/sub/mul/div loops,
// two diagnostics and one branch-join regression. Seven executing tests, all
// on one backend (`x64::compile`, the single-pass tier) and all on one family
// of opcodes. The file's own header calls itself "Differential FP testing",
// which was accurate and is exactly the problem: a differential suite that
// covers `dadd` says nothing about the places where two backends are most
// likely to disagree, which are the ones where the JVMS says something
// SPECIFIC and unobvious:
//
//   * `f2i` / `d2i` SATURATE and map NaN to zero. A `cvttss2si` that is not
//     fixed up produces `0x8000_0000` for all three cases.
//   * `INT_MIN / -1` is `INT_MIN`, not a trap. A raw `IDIV` raises #DE.
//   * Shift counts are MASKED (`& 0x1f` for int, `& 0x3f` for long). x86 masks
//     too, which is why a wrong-width shift hides: `1 << 32` is 1 on both.
//   * `lcmp` is a three-way compare whose answer at `(MIN, MAX)` is where a
//     64-bit subtraction overflows into the wrong sign.
//   * The stack shuffles (`dup2`, `dup_x2`, `dup2_x1`, `dup2_x2`, `swap`,
//     `pop2`) each have two forms and no arithmetic to make an error visible.
//   * `iinc` writes a LOCAL without touching the operand stack, which is
//     precisely the asymmetry that produced this branch's BCE P0.
//   * `monitorenter`/`monitorexit` return values the two tiers treat
//     differently.
//
// Each test below runs BOTH backends over the same bytecode and compares them
// with each other AND with a host-computed anchor, so a shared bug is caught
// as well as a divergence. The anchor is what makes these differential rather
// than merely consistent.
// ===========================================================================

use cratonvm_jit::{
    try_compile_request, CachedBytecodeMethod, CompileRequest, CompiledMethod, DirectHelperTable,
};
use cratonvm_types::{ClassId, ARRAY_LENGTH_OFFSET, HEADER_SIZE};
use std::sync::Arc;

/// The optimizing tier's acceptance gate is a POLICY on top of the routing
/// question these tests ask. Every probe here is a handful of bytecodes that
/// by construction applies no transform the baseline tier lacks, so under the
/// default policy the gate refuses the IR body, `try_compile_request` falls
/// back to single-pass, and the differential compares single-pass with itself
/// — a test that CANNOT FAIL. Same call, same reason, as
/// `ir_vs_singlepass.rs::routing_not_policy`.
fn routing_not_policy() {
    cratonvm_jit::ir_evidence::force_accept_always_for_this_process();
}

/// A `CachedBytecodeMethod` over `code`.
///
/// The VM pads bytecode with two trailing zero bytes and `try_compile`
/// recovers the real length as `code.len() - 2`; without the padding the last
/// two opcodes are dropped and the method emits without a `ret`.
fn cached(
    name: &str,
    descriptor: &str,
    mut code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
) -> CachedBytecodeMethod {
    code.push(0x00);
    code.push(0x00);
    CachedBytecodeMethod {
        declaring_class_id: ClassId::new(1),
        class_name: Arc::from("DiffCorpus"),
        method_name: Arc::from(name),
        method_descriptor: Arc::from(descriptor),
        source_file: None,
        code: Arc::from(code.as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 8,
        max_locals,
        num_params,
        is_synchronized: false,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    }
}

/// Which IR admission gates a probe needs. The optimizing tier bails a whole
/// method on an opcode family it has not been told to admit, and a bail is a
/// silent fall-through to single-pass — which is how a differential quietly
/// stops being one.
#[derive(Clone, Copy, Default)]
struct Gates {
    long: bool,
    fp: bool,
}

fn compile_both(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
    gates: Gates,
    helpers: &JitRuntimeHelpers,
    direct: &DirectHelperTable,
) -> (CompiledMethod, CompiledMethod) {
    routing_not_policy();
    let cm = cached(name, descriptor, code, max_locals, num_params);
    let mut req = CompileRequest::new(&cm, helpers);
    req.direct_helpers = direct;
    req.ir_emit_long = gates.long;
    req.ir_emit_fp = gates.fp;

    req.optimize = true;
    let ir = try_compile_request(&req)
        .unwrap_or_else(|| panic!("{name}: the optimizing (IR) tier failed to compile"));
    req.optimize = false;
    let sp = try_compile_request(&req)
        .unwrap_or_else(|| panic!("{name}: the single-pass tier failed to compile"));
    (ir, sp)
}

/// Run `cases` through both backends and assert IR == single-pass == host.
///
/// `expected` is the full 64-bit result; an int-returning method's cases give
/// a sign-extended `i32`, which is what the JIT's `ireturn` leaves in RAX.
fn differential(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
    gates: Gates,
    cases: &[(Vec<i64>, i64)],
) {
    let helpers = dummy_helpers();
    let direct = DirectHelperTable::EMPTY;
    let (ir, sp) = compile_both(
        name, descriptor, code, max_locals, num_params, gates, &helpers, &direct,
    );
    for (args, expected) in cases {
        // SAFETY: both bodies were produced by the JIT from valid bytecode
        // into executable memory; the i64-arg / i64-ret entry ABI is what
        // `try_call` uses, and no runtime helper is reachable for these
        // methods (they are arithmetic, shuffles and branches only).
        let got_sp = unsafe { sp.try_call(args) }
            .unwrap_or_else(|e| panic!("{name}: single-pass call {args:?}: {e:?}"));
        let got_ir = unsafe { ir.try_call(args) }
            .unwrap_or_else(|e| panic!("{name}: IR call {args:?}: {e:?}"));
        assert_eq!(
            got_ir, got_sp,
            "{name}: the two backends DIVERGE for {args:?} (IR={got_ir}, single-pass={got_sp})",
        );
        assert_eq!(
            got_ir, *expected,
            "{name}: both backends agree and both are WRONG for {args:?}: got {got_ir}, the \
             JVMS says {expected}",
        );
    }
}

/// `differential` for an int-returning method: compares the LOW 32 BITS,
/// which is all an `ireturn` defines.
///
/// Deliberately not a full-width comparison. The high half of RAX after an
/// `ireturn` is not part of the contract, and this branch's second P0 is
/// precisely a case where one tier left it dirty while both tiers agreed on
/// the low 32 bits — a full-width comparison here would turn that into noise
/// on every test in the file instead of a finding in the one test written for
/// it (`the_high_half_of_an_int_may_not_leak_into_an_element_address`, which
/// catches it where it MATTERS: as an address).
fn differential_i32(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
    gates: Gates,
    cases: &[(Vec<i64>, i32)],
) {
    let helpers = dummy_helpers();
    let direct = DirectHelperTable::EMPTY;
    let (ir, sp) = compile_both(
        name, descriptor, code, max_locals, num_params, gates, &helpers, &direct,
    );
    for (args, expected) in cases {
        // SAFETY: as in `differential`.
        let got_sp = unsafe { sp.try_call(args) }
            .unwrap_or_else(|e| panic!("{name}: single-pass call {args:?}: {e:?}"))
            as i32;
        let got_ir = unsafe { ir.try_call(args) }
            .unwrap_or_else(|e| panic!("{name}: IR call {args:?}: {e:?}"))
            as i32;
        assert_eq!(
            got_ir, got_sp,
            "{name}: the two backends DIVERGE for {args:?} (IR={got_ir}, single-pass={got_sp})",
        );
        assert_eq!(
            got_ir, *expected,
            "{name}: both backends agree and both are WRONG for {args:?}: got {got_ir}, the              JVMS says {expected}",
        );
    }
}

// ---------------------------------------------------------------------------
// f2i / d2i saturation (JVMS 6.5 f2i, d2i)
// ---------------------------------------------------------------------------

/// `static int f(int a, int b) { return (int)((float)a / (float)b); }`
///
/// Division by an integer zero would throw; division of two FLOATS by zero
/// does not, which is what makes this the cheapest way to reach ±Infinity and
/// NaN without a constant pool.
#[test]
fn f2i_saturates_at_both_infinities_and_maps_nan_to_zero() {
    // iload_0; i2f; iload_1; i2f; fdiv; f2i; ireturn
    let code = vec![0x1a, 0x86, 0x1b, 0x86, 0x6e, 0x8b, 0xac];
    let host = |a: i32, b: i32| -> i32 {
        // Rust's float-to-int `as` is saturating with NaN -> 0, which is
        // exactly the JVMS rule. That correspondence is the whole anchor.
        (a as f32 / b as f32) as i32
    };
    let cases: Vec<(Vec<i64>, i32)> = [
        (1i64, 0i64), // +Infinity -> Integer.MAX_VALUE
        (-1, 0),      // -Infinity -> Integer.MIN_VALUE
        (0, 0),       // NaN       -> 0
        (7, 2),       // 3.5       -> 3 (truncate toward zero)
        (-7, 2),      // -3.5      -> -3
        (i64::from(i32::MAX), 1),
        (i64::from(i32::MIN), 1),
    ]
    .iter()
    .map(|&(a, b)| (vec![a, b], host(a as i32, b as i32)))
    .collect();
    differential_i32(
        "f2iSat",
        "(II)I",
        code,
        2,
        2,
        Gates {
            fp: true,
            ..Gates::default()
        },
        &cases,
    );
}

/// The `double` twin. Separate test because the two conversions are separate
/// instructions (`cvttss2si` vs `cvttsd2si`) with separately written fix-ups,
/// and a saturation repair applied to one of them is the shape of bug this
/// pair exists to catch.
#[test]
fn d2i_saturates_at_both_infinities_and_maps_nan_to_zero() {
    // iload_0; i2d; iload_1; i2d; ddiv; d2i; ireturn
    let code = vec![0x1a, 0x87, 0x1b, 0x87, 0x6f, 0x8e, 0xac];
    let host = |a: i32, b: i32| -> i32 { (f64::from(a) / f64::from(b)) as i32 };
    let cases: Vec<(Vec<i64>, i32)> = [
        (1i64, 0i64),
        (-1, 0),
        (0, 0),
        (7, 2),
        (-7, 2),
        (i64::from(i32::MAX), 1),
        (i64::from(i32::MIN), 1),
    ]
    .iter()
    .map(|&(a, b)| (vec![a, b], host(a as i32, b as i32)))
    .collect();
    differential_i32(
        "d2iSat",
        "(II)I",
        code,
        2,
        2,
        Gates {
            fp: true,
            long: true,
        },
        &cases,
    );
}

// ---------------------------------------------------------------------------
// INT_MIN / -1 (JVMS 6.5 idiv, irem)
// ---------------------------------------------------------------------------

/// `Integer.MIN_VALUE / -1` overflows and the JVMS defines the result as
/// `Integer.MIN_VALUE` — *not* an exception. A raw `IDIV` on that pair raises
/// #DE, so both tiers must emit an overflow guard, and the guard is easy to
/// write for `idiv` and forget for `irem` (whose answer is 0, from a different
/// register).
#[test]
fn int_division_at_the_overflow_pair_yields_min_value_rather_than_trapping() {
    // iload_0; iload_1; idiv; ireturn
    let code = vec![0x1a, 0x1b, 0x6c, 0xac];
    differential_i32(
        "idivOvf",
        "(II)I",
        code,
        2,
        2,
        Gates::default(),
        &[
            (vec![i64::from(i32::MIN), -1], i32::MIN),
            (vec![i64::from(i32::MIN), 1], i32::MIN),
            (vec![i64::from(i32::MIN), 2], i32::MIN / 2),
            (vec![7, -2], -3),
            (vec![-7, 2], -3),
            (vec![-7, -2], 3),
        ],
    );
}

/// The `irem` half: `MIN % -1` is `0`, and it comes out of RDX rather than
/// RAX, so a guard copied from `idiv` without moving the result is wrong in a
/// way no other input reveals.
#[test]
fn int_remainder_at_the_overflow_pair_yields_zero_rather_than_trapping() {
    // iload_0; iload_1; irem; ireturn
    let code = vec![0x1a, 0x1b, 0x70, 0xac];
    differential_i32(
        "iremOvf",
        "(II)I",
        code,
        2,
        2,
        Gates::default(),
        &[
            (vec![i64::from(i32::MIN), -1], 0),
            (vec![i64::from(i32::MIN), 3], i32::MIN % 3),
            (vec![7, -2], 1),
            (vec![-7, 2], -1),
            (vec![-7, -2], -1),
        ],
    );
}

/// The 64-bit pair. `LONG_MIN / -1` is `LONG_MIN` and `LONG_MIN % -1` is 0,
/// through `CQO`/`IDIV RCX` rather than `CDQ`/`IDIV ECX`.
#[test]
fn long_division_and_remainder_at_the_overflow_pair_do_not_trap() {
    // lload_0; lload_2; ldiv; lreturn
    differential(
        "ldivOvf",
        "(JJ)J",
        vec![0x1e, 0x20, 0x6d, 0xad],
        4,
        2,
        Gates {
            long: true,
            ..Gates::default()
        },
        &[
            (vec![i64::MIN, -1], i64::MIN),
            (vec![i64::MIN, 1], i64::MIN),
            (vec![7, -2], -3),
        ],
    );
    // lload_0; lload_2; lrem; lreturn
    differential(
        "lremOvf",
        "(JJ)J",
        vec![0x1e, 0x20, 0x71, 0xad],
        4,
        2,
        Gates {
            long: true,
            ..Gates::default()
        },
        &[
            (vec![i64::MIN, -1], 0),
            (vec![i64::MIN, 3], i64::MIN % 3),
            (vec![-7, 2], -1),
        ],
    );
}

// ---------------------------------------------------------------------------
// Shift-count masking (JVMS 6.5 ishl/ishr/iushr, lshl/lshr/lushr)
// ---------------------------------------------------------------------------

/// Int shifts use only the low FIVE bits of the count; long shifts use the low
/// SIX. x86 masks by the operand width too, which is exactly why a shift
/// emitted at the wrong width hides: `1 << 32` is `1` under both a correct
/// 32-bit `SHL` and an incorrect 64-bit one. `1 << 33`, and any negative
/// count, separate them.
#[test]
fn int_shift_counts_are_masked_to_five_bits() {
    let cases = |op: fn(i32, u32) -> i32| -> Vec<(Vec<i64>, i32)> {
        [1i64, -1, i64::from(i32::MIN), 0x1234_5678]
            .iter()
            .flat_map(|&v| {
                [0i64, 1, 31, 32, 33, 63, 64, -1, -33]
                    .iter()
                    .map(move |&c| (vec![v, c], op(v as i32, (c as u32) & 31)))
            })
            .collect()
    };
    // iload_0; iload_1; ishl; ireturn
    differential_i32(
        "ishlMask",
        "(II)I",
        vec![0x1a, 0x1b, 0x78, 0xac],
        2,
        2,
        Gates::default(),
        &cases(|v, c| v.wrapping_shl(c)),
    );
    // iload_0; iload_1; ishr; ireturn   (arithmetic)
    differential_i32(
        "ishrMask",
        "(II)I",
        vec![0x1a, 0x1b, 0x7a, 0xac],
        2,
        2,
        Gates::default(),
        &cases(|v, c| v.wrapping_shr(c)),
    );
    // iload_0; iload_1; iushr; ireturn  (logical)
    differential_i32(
        "iushrMask",
        "(II)I",
        vec![0x1a, 0x1b, 0x7c, 0xac],
        2,
        2,
        Gates::default(),
        &cases(|v, c| ((v as u32).wrapping_shr(c)) as i32),
    );
}

/// The long forms take a `long` value and an `int` count — a mixed-width pair
/// the emitter has to keep straight — and mask to six bits.
#[test]
fn long_shift_counts_are_masked_to_six_bits() {
    let gates = Gates {
        long: true,
        ..Gates::default()
    };
    let cases = |op: fn(i64, u32) -> i64| -> Vec<(Vec<i64>, i64)> {
        [1i64, -1, i64::MIN, 0x0123_4567_89ab_cdef]
            .iter()
            .flat_map(|&v| {
                [0i64, 1, 63, 64, 65, 127, -1, -65]
                    .iter()
                    .map(move |&c| (vec![v, c], op(v, (c as u32) & 63)))
            })
            .collect()
    };
    // lload_0; iload_2; lshl; lreturn
    differential(
        "lshlMask",
        "(JI)J",
        vec![0x1e, 0x1c, 0x79, 0xad],
        3,
        2,
        gates,
        &cases(|v, c| v.wrapping_shl(c)),
    );
    // lload_0; iload_2; lshr; lreturn
    differential(
        "lshrMask",
        "(JI)J",
        vec![0x1e, 0x1c, 0x7b, 0xad],
        3,
        2,
        gates,
        &cases(|v, c| v.wrapping_shr(c)),
    );
    // lload_0; iload_2; lushr; lreturn
    differential(
        "lushrMask",
        "(JI)J",
        vec![0x1e, 0x1c, 0x7d, 0xad],
        3,
        2,
        gates,
        &cases(|v, c| ((v as u64).wrapping_shr(c)) as i64),
    );
}

// ---------------------------------------------------------------------------
// lcmp at the extremes (JVMS 6.5 lcmp)
// ---------------------------------------------------------------------------

/// `lcmp` is a three-way compare pushing -1 / 0 / 1. The tempting lowering is
/// `SUB` plus a sign test, and it is wrong at exactly the pairs where the
/// 64-bit subtraction overflows: `(MIN, MAX)` and `(MAX, MIN)`.
#[test]
fn lcmp_answers_three_ways_even_where_a_subtraction_would_overflow() {
    // lload_0; lload_2; lcmp; ireturn
    let code = vec![0x1e, 0x20, 0x94, 0xac];
    let cases: Vec<(Vec<i64>, i32)> = [
        (i64::MIN, i64::MAX),
        (i64::MAX, i64::MIN),
        (i64::MIN, i64::MIN),
        (i64::MAX, i64::MAX),
        (i64::MIN, 0),
        (0, i64::MIN),
        (i64::MAX, 0),
        (0, i64::MAX),
        (-1, 0),
        (0, -1),
        (-1, -1),
        (i64::MIN, -1),
        (-1, i64::MIN),
    ]
    .iter()
    .map(|&(a, b)| {
        let want = match a.cmp(&b) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        };
        (vec![a, b], want)
    })
    .collect();
    differential_i32(
        "lcmpExtremes",
        "(JJ)I",
        code,
        4,
        2,
        Gates {
            long: true,
            ..Gates::default()
        },
        &cases,
    );
}

// ---------------------------------------------------------------------------
// The stack shuffles (JVMS 6.5 swap, pop2, dup2, dup_x2, dup2_x1, dup2_x2)
// ---------------------------------------------------------------------------
//
// Every reduction below uses `isub` and `imul` rather than `iadd`, on purpose:
// addition is commutative, so a shuffle that produces the right MULTISET in
// the wrong ORDER still yields the right sum. These sequences do not.

/// `swap` exchanges the top two category-1 values.
#[test]
fn swap_exchanges_the_top_two_operands() {
    // iload_0; iload_1; swap; isub; ireturn   =>  b - a
    let code = vec![0x1a, 0x1b, 0x5f, 0x64, 0xac];
    differential_i32(
        "swapOp",
        "(II)I",
        code,
        2,
        2,
        Gates::default(),
        &[
            (vec![3, 5], 2),
            (vec![5, 3], -2),
            (vec![i64::from(i32::MIN), 1], 1i32.wrapping_sub(i32::MIN)),
        ],
    );
}

/// `pop2` in its form-1 shape removes TWO category-1 values, not one.
#[test]
fn pop2_removes_two_category_one_operands() {
    // iload_0; iload_1; iload_2; pop2; ireturn  =>  a
    let code = vec![0x1a, 0x1b, 0x1c, 0x58, 0xac];
    differential_i32(
        "pop2Op",
        "(III)I",
        code,
        3,
        3,
        Gates::default(),
        &[(vec![3, 5, 7], 3), (vec![-1, 0, 0], -1)],
    );
}

/// `dup2` in its form-2 shape duplicates the top two category-1 values as a
/// pair: `a b -> a b a b`.
#[test]
fn dup2_duplicates_the_top_two_operands_as_a_pair() {
    // iload_0; iload_1; dup2; isub; isub; isub; ireturn
    //   [a,b] -> [a,b,a,b] -> [a,b,a-b] -> [a,2b-a] -> [2a-2b]
    let code = vec![0x1a, 0x1b, 0x5c, 0x64, 0x64, 0x64, 0xac];
    let host = |a: i32, b: i32| a.wrapping_sub(b).wrapping_mul(2);
    differential_i32(
        "dup2Op",
        "(II)I",
        code,
        2,
        2,
        Gates::default(),
        &[
            (vec![3, 5], host(3, 5)),
            (vec![5, 3], host(5, 3)),
            (vec![i64::from(i32::MAX), -1], host(i32::MAX, -1)),
        ],
    );
}

/// `dup_x2` form 1: `c b a -> a c b a` (bottom-to-top), i.e. the top value is
/// re-inserted THREE places down.
#[test]
fn dup_x2_reinserts_the_top_operand_below_two_others() {
    // iload_0; iload_1; iload_2; dup_x2; imul; isub; isub; ireturn
    //   [a,b,c] -> [c,a,b,c] -> [c,a,b*c] -> [c,a-b*c] -> [c-a+b*c]
    let code = vec![0x1a, 0x1b, 0x1c, 0x5b, 0x68, 0x64, 0x64, 0xac];
    let host = |a: i32, b: i32, c: i32| c.wrapping_sub(a).wrapping_add(b.wrapping_mul(c));
    differential_i32(
        "dupX2Op",
        "(III)I",
        code,
        3,
        3,
        Gates::default(),
        &[
            (vec![3, 5, 7], host(3, 5, 7)),
            (vec![7, 5, 3], host(7, 5, 3)),
            (vec![-1, -2, -3], host(-1, -2, -3)),
        ],
    );
}

/// `dup2_x1` form 2: `c b a -> b a c b a`.
#[test]
fn dup2_x1_reinserts_the_top_pair_below_one_other() {
    // iload_0; iload_1; iload_2; dup2_x1; imul; isub; isub; isub; ireturn
    //   [a,b,c] -> [b,c,a,b,c] -> [b,c,a,b*c] -> [b,c,a-b*c]
    //           -> [b, c-a+b*c] -> [b-c+a-b*c]
    let code = vec![0x1a, 0x1b, 0x1c, 0x5d, 0x68, 0x64, 0x64, 0x64, 0xac];
    let host = |a: i32, b: i32, c: i32| {
        b.wrapping_sub(c)
            .wrapping_add(a)
            .wrapping_sub(b.wrapping_mul(c))
    };
    differential_i32(
        "dup2X1Op",
        "(III)I",
        code,
        3,
        3,
        Gates::default(),
        &[
            (vec![3, 5, 7], host(3, 5, 7)),
            (vec![7, 5, 3], host(7, 5, 3)),
            (vec![-1, -2, -3], host(-1, -2, -3)),
        ],
    );
}

/// `dup2_x2` form 1: `d c b a -> b a d c b a`, all four category-1.
#[test]
fn dup2_x2_reinserts_the_top_pair_below_two_others() {
    // iload_0..3; dup2_x2; imul; isub; isub; isub; isub; ireturn
    //   [a,b,c,d] -> [c,d,a,b,c,d] -> [c,d,a,b,c*d] -> [c,d,a,b-c*d]
    //             -> [c,d,a-b+c*d] -> [c, d-a+b-c*d] -> [c-d+a-b+c*d]
    let code = vec![
        0x1a, 0x1b, 0x1c, 0x1d, 0x5e, 0x68, 0x64, 0x64, 0x64, 0x64, 0xac,
    ];
    let host = |a: i32, b: i32, c: i32, d: i32| {
        c.wrapping_sub(d)
            .wrapping_add(a)
            .wrapping_sub(b)
            .wrapping_add(c.wrapping_mul(d))
    };
    differential_i32(
        "dup2X2Op",
        "(IIII)I",
        code,
        4,
        4,
        Gates::default(),
        &[
            (vec![3, 5, 7, 11], host(3, 5, 7, 11)),
            (vec![11, 7, 5, 3], host(11, 7, 5, 3)),
            (vec![-1, -2, -3, -4], host(-1, -2, -3, -4)),
        ],
    );
}

// ---------------------------------------------------------------------------
// iinc (JVMS 6.5 iinc, wide iinc)
// ---------------------------------------------------------------------------

/// `iinc` writes a LOCAL and touches the operand stack not at all. That
/// asymmetry is not a curiosity: modelling `iinc` as a no-op — true of the
/// stack, false of the value — is exactly the premise that produced this
/// branch's bounds-check-elimination P0.
///
/// The increment wraps on overflow like every other int arithmetic in the JVM,
/// and the immediate is a SIGNED byte.
#[test]
fn iinc_adds_a_signed_byte_to_a_local_and_wraps() {
    // iinc 0, 127; iload_0; ireturn
    let plus = vec![0x84, 0x00, 0x7f, 0x1a, 0xac];
    differential_i32(
        "iincPlus",
        "(I)I",
        plus,
        1,
        1,
        Gates::default(),
        &[
            (vec![0], 127),
            (vec![i64::from(i32::MAX)], i32::MAX.wrapping_add(127)),
            (vec![-127], 0),
        ],
    );
    // iinc 0, -128; iload_0; ireturn
    let minus = vec![0x84, 0x00, 0x80, 0x1a, 0xac];
    differential_i32(
        "iincMinus",
        "(I)I",
        minus,
        1,
        1,
        Gates::default(),
        &[
            (vec![0], -128),
            (vec![i64::from(i32::MIN)], i32::MIN.wrapping_sub(128)),
            (vec![128], 0),
        ],
    );
}

/// The `wide` form takes a two-byte local index and a two-byte SIGNED
/// constant, which is a different decode path from the narrow form and shares
/// none of its immediate handling.
#[test]
fn wide_iinc_adds_a_signed_short_to_a_local() {
    // wide iinc 0, 32767; iload_0; ireturn
    let plus = vec![0xc4, 0x84, 0x00, 0x00, 0x7f, 0xff, 0x1a, 0xac];
    differential_i32(
        "wideIincPlus",
        "(I)I",
        plus,
        1,
        1,
        Gates::default(),
        &[
            (vec![0], 32767),
            (vec![i64::from(i32::MAX)], i32::MAX.wrapping_add(32767)),
        ],
    );
    // wide iinc 0, -32768; iload_0; ireturn
    let minus = vec![0xc4, 0x84, 0x00, 0x00, 0x80, 0x00, 0x1a, 0xac];
    differential_i32(
        "wideIincMinus",
        "(I)I",
        minus,
        1,
        1,
        Gates::default(),
        &[
            (vec![0], -32768),
            (vec![i64::from(i32::MIN)], i32::MIN.wrapping_sub(32768)),
        ],
    );
}

// ---------------------------------------------------------------------------
// monitorenter / monitorexit
// ---------------------------------------------------------------------------

/// What the monitor helpers were asked and answered, in call order.
static MONITOR_LOG: std::sync::Mutex<Vec<(&'static str, i64)>> = std::sync::Mutex::new(Vec::new());

/// `jit_monitor_enter(vm, obj) -> possibly-remapped obj | i64::MIN`.
///
/// Returns the object UNCHANGED, which is the uncontended answer and the only
/// one a test without a collector can make claims about.
///
/// # Safety
/// Called from compiled code with the entry ABI; touches only a mutex.
unsafe extern "C" fn monitor_enter_stub(_vm: i64, obj: i64) -> i64 {
    if let Ok(mut log) = MONITOR_LOG.lock() {
        log.push(("enter", obj));
    }
    obj
}

/// `jit_monitor_exit(vm, obj) -> 1 | i64::MIN`. The `1` is the point: it is
/// NOT the object, and a tier that stored it back into the receiver's home
/// slot would make the `aload` after the unlock read the address `1`.
///
/// # Safety
/// As [`monitor_enter_stub`].
unsafe extern "C" fn monitor_exit_stub(_vm: i64, obj: i64) -> i64 {
    if let Ok(mut log) = MONITOR_LOG.lock() {
        log.push(("exit", obj));
    }
    1
}

/// `static Object f(Object o) { Object m = o; synchronized (m) { } return m; }`
///
/// The receiver is read back from the SAME local the monitor ops used, which
/// is what makes this a regression test rather than a smoke test: the IR tier
/// stores `monitor_enter`'s return into that slot, and storing `monitor_exit`'s
/// return there instead put the address `1` into it — a second
/// `synchronized (this)` in the same method then read `this` back as `1` and
/// the oop map published it (`ir_lower.rs`, the `Op::MonitorEnter` arm).
///
/// The two tiers reach the helper differently — the IR tier through
/// `helpers.monitor_enter`, the single-pass tier through
/// `direct_helpers.monitor_enter` — so both tables are wired here.
#[test]
fn a_monitor_pair_locks_and_unlocks_the_same_object_and_leaves_its_slot_intact() {
    // aload_0; astore_1; aload_1; monitorenter; aload_1; monitorexit;
    // aload_1; areturn
    let code = vec![0x2a, 0x4c, 0x2b, 0xc2, 0x2b, 0xc3, 0x2b, 0xb0];
    let mut helpers = dummy_helpers();
    helpers.monitor_enter = monitor_enter_stub as *const () as usize;
    helpers.monitor_exit = monitor_exit_stub as *const () as usize;
    let direct = DirectHelperTable {
        monitor_enter: monitor_enter_stub as *const () as usize,
        monitor_exit: monitor_exit_stub as *const () as usize,
        ..DirectHelperTable::EMPTY
    };
    let (ir, sp) = compile_both(
        "monitorPair",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        code,
        2,
        1,
        Gates::default(),
        &helpers,
        &direct,
    );

    // A plausible, 8-aligned, non-null "object pointer". The JIT only moves it
    // and hands it to the stubs.
    const OBJ: i64 = 0x0000_7fff_1234_5678;
    let dummy_vm = [0u8; 64];
    for (which, body) in [("single-pass", &sp), ("IR", &ir)] {
        MONITOR_LOG.lock().expect("log").clear();
        // SAFETY: the body was produced by the JIT from valid bytecode; the
        // only helpers it can reach are the two monitor stubs wired above,
        // and both are `extern "C"` with the declared signature.
        let got = unsafe { body.try_call_with_context(dummy_vm.as_ptr() as i64, &[OBJ]) }
            .unwrap_or_else(|e| panic!("{which}: monitorPair call: {e:?}"));
        assert_eq!(
            got, OBJ,
            "{which}: the object read back after the unlock is not the object that was \
             locked. `monitor_exit` answers 1, not the receiver — a tier that stores its \
             return into the receiver's home slot returns that 1.",
        );
        let log = MONITOR_LOG.lock().expect("log").clone();
        assert_eq!(
            log,
            vec![("enter", OBJ), ("exit", OBJ)],
            "{which}: the monitor helpers must be called once each, enter first, both with \
             the locked object",
        );
    }
}

// ===========================================================================
// Regression tests for the two out-of-bounds element-address holes fixed on
// this branch (`fix(jit): two out-of-bounds element-address holes in the
// optimizing tier`).
//
// Both are OPTIMIZING-TIER bugs whose single-pass twin is correct, which is
// why `ir_vs_singlepass.rs` could not see either: in the second, both tiers
// agree on the low 32 bits and only the ADDRESS diverges. So neither is a
// differential in the "two backends disagree" sense. What each test below
// pins is the *observable* consequence — a throw that must happen, and memory
// that must not be written.
// ===========================================================================

/// A synthetic `int[]` with a guard region after the last element.
///
/// The layout is the one the inline element codegen reads and nothing else:
/// the `i32` element count at `ARRAY_LENGTH_OFFSET`, then packed 4-byte
/// elements from `HEADER_SIZE`. `words` backing storage makes the base
/// 8-aligned, which the walker and the element scaling both assume.
///
/// The guard is the point. An assertion that an exception was thrown proves
/// only that the check fired; it does not prove the store did not ALSO happen,
/// and "threw but wrote anyway" is a live shape — the elided-check arm forms
/// the address from the same register whether or not a guard precedes it.
struct GuardedIntArray {
    words: Vec<u64>,
    len: usize,
}

impl GuardedIntArray {
    /// `len` elements of `0`, followed by `GUARD_ELEMS` element slots that
    /// must stay zero.
    const GUARD_ELEMS: usize = 16;

    fn new(len: usize) -> Self {
        let bytes = HEADER_SIZE + (len + Self::GUARD_ELEMS) * 4;
        let mut me = GuardedIntArray {
            words: vec![0u64; bytes.div_ceil(8)],
            len,
        };
        // SAFETY: `ARRAY_LENGTH_OFFSET + 4 <= HEADER_SIZE <= bytes`, and the
        // allocation is at least `bytes` long.
        unsafe {
            std::ptr::write_unaligned(me.base().add(ARRAY_LENGTH_OFFSET) as *mut i32, len as i32);
        }
        me
    }

    fn base(&mut self) -> *mut u8 {
        self.words.as_mut_ptr() as *mut u8
    }

    fn handle(&mut self) -> i64 {
        self.base() as i64
    }

    fn set(&mut self, index: usize, value: i32) {
        assert!(index < self.len, "fixture bug: {index} is out of range");
        // SAFETY: `index < len`, so the slot is inside the element region.
        unsafe {
            std::ptr::write_unaligned(self.base().add(HEADER_SIZE + index * 4) as *mut i32, value);
        }
    }

    fn get(&mut self, index: usize) -> i32 {
        // SAFETY: callers pass an index inside the element-plus-guard region.
        unsafe { std::ptr::read_unaligned(self.base().add(HEADER_SIZE + index * 4) as *const i32) }
    }

    /// Every guard slot, which must be zero for the whole of a run that was
    /// supposed to throw before reaching them.
    fn guards_are_untouched(&mut self) -> bool {
        (self.len..self.len + Self::GUARD_ELEMS).all(|i| self.get(i) == 0)
    }
}

/// What `jit_throw_aioobe(index, length, array_ptr, bytecode_pc)` was handed.
static AIOOBE_LOG: std::sync::Mutex<Vec<(i64, i64)>> = std::sync::Mutex::new(Vec::new());

/// Serialises the three tests that use [`AIOOBE_LOG`].
///
/// The log is process-global and each of those tests clears it, runs a body,
/// then reads it back. `cargo test` runs them on separate threads in one
/// process, so without this they interleave: one test's `clear()` lands
/// between another's call and its read, and the reader sees an empty log —
/// which is indistinguishable from the bounds check having been elided, i.e.
/// exactly the failure the BCE regression test exists to report. It failed
/// that way in a full-suite run while passing when filtered to itself.
///
/// Held across clear-call-assert, so the whole critical section is one
/// test's. `unwrap_or_else(PoisonError::into_inner)` because a panic in one of
/// these tests must surface as that test's own failure, not as a poisoned
/// mutex turning the other two into cascading noise.
static AIOOBE_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Stands in for `jit_throw_aioobe`. The real helper publishes a pending
/// `ArrayIndexOutOfBoundsException` and answers with the `i64::MIN` deopt
/// sentinel; this one records `(index, length)` and answers the same way, so
/// the compiled body takes its exception path exactly as it would in the VM.
///
/// # Safety
/// Called from compiled code with the entry ABI; touches only a mutex.
unsafe extern "C" fn throw_aioobe_stub(index: i64, length: i64, _array: i64, _pc: i64) -> i64 {
    if let Ok(mut log) = AIOOBE_LOG.lock() {
        log.push((index, length));
    }
    i64::MIN
}

/// Compile one method through the OPTIMIZING tier and prove the IR backend
/// actually produced the body. A silent fall-through to single-pass would make
/// either regression test below vacuous — both bugs are IR-tier only.
fn compile_optimizing(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
    helpers: &JitRuntimeHelpers,
) -> CompiledMethod {
    routing_not_policy();
    let cm = cached(name, descriptor, code, max_locals, num_params);
    let mut req = CompileRequest::new(&cm, helpers);
    req.optimize = true;
    let body = try_compile_request(&req)
        .unwrap_or_else(|| panic!("{name}: the optimizing tier failed to compile"));
    assert!(
        body.used_ir_backend,
        "{name}: this test is about the IR arm — a silent single-pass fallback would make \
         it vacuous, because the bug it pins does not exist in the single-pass tier",
    );
    body
}

/// The single-pass arm of [`compile_optimizing`], for a defect that lives in
/// that tier.
///
/// The two P0 regressions below are in OPPOSITE tiers and it matters which
/// backend compiles each. The dirty-high-half defect is the IR lowerer's
/// (`Op::And`/`Or`/`Xor` hard-coding REX.W) and cannot be reached from the
/// single-pass emitter, which does 32-bit + `MOVSXD`. The BCE `iinc` defect is
/// the reverse: it is in `x64/bce.rs`, the SINGLE-PASS bounds-check
/// elimination, and the IR tier has its own `ir_check_elim` instead.
///
/// The mechanism differs too, which is what makes running the BCE test on the
/// wrong tier silently vacuous rather than loudly wrong. An out-of-range index
/// in the IR tier reaches `emit_deopt_unless(.., DeoptReason::BoundsCheck)` and
/// leaves through the deopt sentinel; it never calls `throw_aioobe`, so
/// `AIOOBE_LOG` stays empty whether or not the check was elided.
fn compile_single_pass(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
    helpers: &JitRuntimeHelpers,
) -> CompiledMethod {
    routing_not_policy();
    let cm = cached(name, descriptor, code, max_locals, num_params);
    let mut req = CompileRequest::new(&cm, helpers);
    req.optimize = false;
    let body = try_compile_request(&req)
        .unwrap_or_else(|| panic!("{name}: the single-pass tier failed to compile"));
    assert!(
        !body.used_ir_backend,
        "{name}: this test is about the SINGLE-PASS arm — the bounds-check elimination it          pins lives in `x64/bce.rs`, and the IR tier neither uses that pass nor reports an          out-of-range index through `throw_aioobe`",
    );
    body
}

/// Helpers with the AIOOBE thrower wired and nothing else reachable.
fn bounds_helpers() -> JitRuntimeHelpers {
    let mut h = dummy_helpers();
    h.throw_aioobe = throw_aioobe_stub as *const () as usize;
    h
}

/// P0-1 — `while (i < n) { i++; a[i] = i; }` with `f(new int[8], 8)` must
/// throw `ArrayIndexOutOfBoundsException` at index 8, and must not write there.
///
/// `analyze_array_access_operands` modelled `iinc` as a no-op. That is true of
/// the operand STACK and false of the VALUE, so an access reading the
/// induction variable AFTER an in-body advance was described to the
/// bounds-check proof as the bare induction variable. The header guard
/// `a.length >= n` passed, the per-element check was elided, and the last
/// iteration stored one past the end.
///
/// The ordinary javac counted loop — `a[i] = i; i++` — keeps a displacement of
/// zero and must still be eligible for elision, which the companion test below
/// pins: a fix that simply refuses to eliminate anything would pass this test
/// and lose the optimization.
#[test]
fn an_index_read_after_an_in_body_iinc_is_still_bounds_checked() {
    // static void f(int[] a, int n) { int i = 0; while (i < n) { i++; a[i] = i; } }
    //
    //  0: iconst_0        03
    //  1: istore_2        3d
    //  2: iload_2         1c        <- loop condition
    //  3: iload_1         1b
    //  4: if_icmpge +13   a2 00 0d  -> 17
    //  7: iinc 2, 1       84 02 01
    // 10: aload_0         2a
    // 11: iload_2         1c
    // 12: iload_2         1c
    // 13: iastore         4f
    // 14: goto -12        a7 ff f4  -> 2
    // 17: return          b1
    let code = vec![
        0x03, 0x3d, 0x1c, 0x1b, 0xa2, 0x00, 0x0d, 0x84, 0x02, 0x01, 0x2a, 0x1c, 0x1c, 0x4f, 0xa7,
        0xff, 0xf4, 0xb1,
    ];
    let helpers = bounds_helpers();
    let body = compile_single_pass("iincBce", "([II)V", code, 3, 2, &helpers);

    let mut a = GuardedIntArray::new(8);
    let _aioobe_gate = AIOOBE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    AIOOBE_LOG.lock().expect("log").clear();
    let dummy_vm = [0u8; 64];
    // SAFETY: the body was produced by the JIT from valid bytecode; `a` is a
    // correctly laid-out array fixture and the only helper the body can reach
    // is the AIOOBE thrower wired above.
    let _ = unsafe { body.try_call_with_context(dummy_vm.as_ptr() as i64, &[a.handle(), 8]) };

    let log = AIOOBE_LOG.lock().expect("log").clone();
    assert_eq!(
        log,
        vec![(8, 8)],
        "the store at i == 8 must raise ArrayIndexOutOfBoundsException exactly once, with \
         index 8 against length 8. An empty log means the per-element check was ELIDED on \
         an index the loop's exit test never bounded — `i < n` bounds the value BEFORE the \
         `iinc`, not after it.",
    );
    assert!(
        a.guards_are_untouched(),
        "the out-of-range store wrote past the end of the array. Throwing is not enough: \
         the elided-check arm forms the element address from the same register whether or \
         not a guard precedes it, so a fix that adds the throw without stopping the store \
         leaves the memory corruption in place. Guard slots: {:?}",
        (a.len..a.len + GuardedIntArray::GUARD_ELEMS)
            .map(|i| a.get(i))
            .collect::<Vec<_>>(),
    );
    // Everything below index 8 is written on the way, which is what a plain
    // Java run would also have done before throwing.
    for i in 1..8 {
        assert_eq!(
            a.get(i),
            i as i32,
            "a[{i}] was not written before the throw"
        );
    }
}

/// The control for the test above: the ORDINARY counted loop, where the index
/// is read BEFORE the advance, must still run to completion without reaching
/// the thrower.
///
/// A displacement of zero is the common case, and a "fix" that refused every
/// elision would satisfy the regression test while silently costing the
/// optimization on every `for` loop javac emits. This is what says the fix is
/// narrow.
#[test]
fn the_ordinary_counted_loop_still_runs_without_a_bounds_failure() {
    // static void f(int[] a, int n) { int i = 0; while (i < n) { a[i] = i; i++; } }
    //
    //  0: iconst_0        03
    //  1: istore_2        3d
    //  2: iload_2         1c
    //  3: iload_1         1b
    //  4: if_icmpge +13   a2 00 0d  -> 17
    //  7: aload_0         2a
    //  8: iload_2         1c
    //  9: iload_2         1c
    // 10: iastore         4f
    // 11: iinc 2, 1       84 02 01
    // 14: goto -12        a7 ff f4  -> 2
    // 17: return          b1
    let code = vec![
        0x03, 0x3d, 0x1c, 0x1b, 0xa2, 0x00, 0x0d, 0x2a, 0x1c, 0x1c, 0x4f, 0x84, 0x02, 0x01, 0xa7,
        0xff, 0xf4, 0xb1,
    ];
    let helpers = bounds_helpers();
    let body = compile_single_pass("counted", "([II)V", code, 3, 2, &helpers);

    let mut a = GuardedIntArray::new(8);
    let _aioobe_gate = AIOOBE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    AIOOBE_LOG.lock().expect("log").clear();
    let dummy_vm = [0u8; 64];
    // SAFETY: as above.
    let _ = unsafe { body.try_call_with_context(dummy_vm.as_ptr() as i64, &[a.handle(), 8]) };

    assert!(
        AIOOBE_LOG.lock().expect("log").is_empty(),
        "the ordinary `a[i] = i; i++` loop is fully in range and must not reach the thrower",
    );
    assert!(
        a.guards_are_untouched(),
        "the in-range loop wrote past the end"
    );
    for i in 0..8 {
        assert_eq!(a.get(i), i as i32, "a[{i}]");
    }
}

/// P0-2 — an `int` carrying a dirty high half must not reach an element
/// address.
///
/// `Op::And`/`Or`/`Xor` hard-coded `REX.W` while `Add`/`Sub`/`Mul`/`Shl` passed
/// `node.ty != IrType::Int`. The tier documents "int slot = upper half ZEROED",
/// but `F2I`, `D2I`, `LCmp`, `FCmp`, `iaload`, a negative `Const` and a
/// negative `Param` all SIGN-extend, so a 64-bit XOR of one of each produced a
/// word that is neither extension of its own low half:
///
/// ```text
/// int t = p + q;  int i = src[0] ^ t;  return dst[i];   // src[0]=-5, t=-3
/// ```
///
/// `0xFFFF_FFFB ^ 0xFFFF_FFFD` is `6` in 32 bits and
/// `0xFFFF_FFFF_0000_0006` in 64. The bounds check is a 32-bit `CMP ECX,R10D`,
/// which `6` passes; the element access scales all 64 bits of RCX as the SIB
/// index, addressing `dst - 17 GB`. The `iastore` form of the same shape was
/// an arbitrary WRITE.
///
/// The single-pass tier is unaffected (32-bit ops plus `MOVSXD`), so
/// `ir_vs_singlepass` could not see this: both tiers agree on the low 32 bits
/// and only the address diverges. That is why this test asserts the VALUE READ
/// rather than comparing the tiers.
#[test]
fn the_high_half_of_an_int_may_not_leak_into_an_element_address() {
    // static int f(int[] src, int[] dst, int p, int q) {
    //     int t = p + q;
    //     int i = src[0] ^ t;
    //     return dst[i];
    // }
    //
    //  0: iload_2        1c        (p)
    //  1: iload_3        1d        (q)
    //  2: iadd           60
    //  3: istore 4       36 04     (t)
    //  5: aload_0        2a        (src)
    //  6: iconst_0       03
    //  7: iaload         2e
    //  8: iload 4        15 04
    // 10: ixor           82
    // 11: istore 5       36 05     (i)
    // 13: aload_1        2b        (dst)
    // 14: iload 5        15 05
    // 16: iaload         2e
    // 17: ireturn        ac
    let code = vec![
        0x1c, 0x1d, 0x60, 0x36, 0x04, 0x2a, 0x03, 0x2e, 0x15, 0x04, 0x82, 0x36, 0x05, 0x2b, 0x15,
        0x05, 0x2e, 0xac,
    ];
    let helpers = bounds_helpers();
    let body = compile_optimizing("dirtyHighHalf", "([I[III)I", code, 6, 4, &helpers);

    let mut src = GuardedIntArray::new(1);
    src.set(0, -5);
    let mut dst = GuardedIntArray::new(8);
    for i in 0..8 {
        dst.set(i, (i as i32) * 10);
    }

    let _aioobe_gate = AIOOBE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    AIOOBE_LOG.lock().expect("log").clear();
    let dummy_vm = [0u8; 64];
    // p + q = -3, so `src[0] ^ t` is `-5 ^ -3` = 6 as an int. Anything other
    // than `dst[6]` means the address was formed from more than 32 bits.
    //
    // SAFETY: both fixtures are correctly laid out and the computed index (6)
    // is in range for `dst`. If the index were NOT normalised the access is
    // wild — which is the bug, and is why this test is the one that would
    // segfault rather than mis-answer.
    let got = unsafe {
        body.try_call_with_context(
            dummy_vm.as_ptr() as i64,
            &[src.handle(), dst.handle(), -1, -2],
        )
    }
    .expect("dirtyHighHalf call");

    assert!(
        AIOOBE_LOG.lock().expect("log").is_empty(),
        "index 6 is in range for an 8-element array; reaching the thrower means the bounds \
         check saw something other than 6",
    );
    assert_eq!(
        got as i32, 60,
        "`dst[src[0] ^ (p + q)]` must read dst[6]. A 64-bit AND/OR/XOR over operands that \
         sign-extend produces a word that is neither extension of its own low half; the \
         32-bit bounds check passes it and the SIB index scales all 64 bits of it.",
    );
    assert!(
        dst.guards_are_untouched() && src.guards_are_untouched(),
        "a read-only probe must not have written anything",
    );
}

/// P0-2, the WRITE form. The read above addressed `dst - 17 GB`; the same
/// dirty high half under `iastore` is an arbitrary store, which is the half of
/// the defect that corrupts memory rather than reading it.
///
/// Kept separate from the read because the two element paths are separate
/// lowerings, and `emit_array_null_bounds_guards_for`'s `MOV ECX, ECX` has to
/// hold on both: a normalisation added to the load arm alone would leave this
/// one writing wild.
#[test]
fn the_high_half_of_an_int_may_not_leak_into_an_element_store_address() {
    // static void f(int[] src, int[] dst, int p, int q, int v) {
    //     int t = p + q;
    //     int i = src[0] ^ t;
    //     dst[i] = v;
    // }
    //
    //  0: iload_2        1c        (p)
    //  1: iload_3        1d        (q)
    //  2: iadd           60
    //  3: istore 5       36 05     (t)
    //  5: aload_0        2a        (src)
    //  6: iconst_0       03
    //  7: iaload         2e
    //  8: iload 5        15 05
    // 10: ixor           82
    // 11: istore 6       36 06     (i)
    // 13: aload_1        2b        (dst)
    // 14: iload 6        15 06
    // 16: iload 4        15 04     (v)
    // 18: iastore        4f
    // 19: return         b1
    let code = vec![
        0x1c, 0x1d, 0x60, 0x36, 0x05, 0x2a, 0x03, 0x2e, 0x15, 0x05, 0x82, 0x36, 0x06, 0x2b, 0x15,
        0x06, 0x15, 0x04, 0x4f, 0xb1,
    ];
    let helpers = bounds_helpers();
    let body = compile_optimizing("dirtyHighHalfStore", "([I[IIII)V", code, 7, 5, &helpers);

    let mut src = GuardedIntArray::new(1);
    src.set(0, -5);
    let mut dst = GuardedIntArray::new(8);

    let _aioobe_gate = AIOOBE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    AIOOBE_LOG.lock().expect("log").clear();
    let dummy_vm = [0u8; 64];
    // p + q = -3, so the index is `-5 ^ -3` = 6. An unnormalised index makes
    // this a store to `dst - 17 GB`, which is why a regression here is a crash
    // and not an assertion failure.
    //
    // SAFETY: both fixtures are correctly laid out and index 6 is in range.
    let _ = unsafe {
        body.try_call_with_context(
            dummy_vm.as_ptr() as i64,
            &[src.handle(), dst.handle(), -1, -2, 77],
        )
    };

    assert!(
        AIOOBE_LOG.lock().expect("log").is_empty(),
        "index 6 is in range for an 8-element array; reaching the thrower means the bounds \
         check saw something other than 6",
    );
    for i in 0..8 {
        assert_eq!(
            dst.get(i),
            if i == 6 { 77 } else { 0 },
            "dst[{i}]: `dst[src[0] ^ (p + q)] = v` must write dst[6] and nothing else",
        );
    }
    assert!(
        dst.guards_are_untouched() && src.guards_are_untouched(),
        "the store reached a guard slot",
    );
}

// ---------------------------------------------------------------------------
// The admission funnel
// ---------------------------------------------------------------------------

/// One row of `ir_evidence::admission_funnel`. The counters are process-wide
/// and the tests in this binary run concurrently, so callers compare a
/// before/after pair with `>`, never with `==`.
fn funnel_row(name: &str) -> u64 {
    cratonvm_jit::ir_evidence::admission_funnel()
        .into_iter()
        .find(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("no admission-funnel row named {name}"))
        .1
}

/// An optimizing request the IR tier publishes is counted at `published`.
///
/// The funnel exists because every OTHER exit produces the same observable as
/// this one failing: a single-pass body. Without a row that moves when the
/// tier does publish, a funnel that counts nothing would read as "the tier
/// refuses everything", which is the opposite of silence and just as wrong.
#[test]
fn a_published_ir_body_is_counted_at_the_published_exit() {
    let before = funnel_row("published");
    // static int f(int a, int b) { return a ^ b; }
    let code = vec![0x1a, 0x1b, 0x82, 0xac];
    let _body = compile_optimizing("funnelPublished", "(II)I", code, 2, 2, &dummy_helpers());
    assert!(
        funnel_row("published") > before,
        "a body the optimizing tier produced was not counted as published: {:?}",
        cratonvm_jit::ir_evidence::admission_funnel(),
    );
}

/// A method over `IR_MAX_BYTECODE_SIZE` is refused by `ir_compatible_sized`
/// and must be counted there, not folded into the other gates -- the byte
/// budget is one of the four caps the funnel exists to put a number on.
#[test]
fn an_over_budget_method_is_counted_at_the_ir_compatible_exit() {
    routing_not_policy();
    let before = funnel_row("refused-ir-compatible");
    // `nop` x (budget + 1), then `iconst_0; ireturn`.
    let mut code = vec![0x00u8; cratonvm_jit::ir::IR_MAX_BYTECODE_SIZE + 1];
    code.extend_from_slice(&[0x03, 0xac]);
    let cm = cached("funnelOverBudget", "()I", code, 0, 0);
    let helpers = dummy_helpers();
    let mut req = CompileRequest::new(&cm, &helpers);
    req.optimize = true;
    if let Some(body) = try_compile_request(&req) {
        assert!(
            !body.used_ir_backend,
            "an over-budget method reached the IR backend"
        );
    }
    assert!(
        funnel_row("refused-ir-compatible") > before,
        "a method over IR_MAX_BYTECODE_SIZE was not counted at the ir_compatible exit: {:?}",
        cratonvm_jit::ir_evidence::admission_funnel(),
    );
}

// ---------------------------------------------------------------------------
// The guard token edge, and the silence it could cause
// ---------------------------------------------------------------------------

/// A method carrying a guard must still reach the IR tier when the token edge
/// is emitted.
///
/// `CRATONVM_JIT_IR_GUARD_TOKEN=1` makes `IrBuilder` append a token edge naming
/// the guard that licenses a node, so `LoopGuards::permits_hoist` can see it.
/// Every consumer of the IR was taught the wider shape; the producer is gated
/// because of what happens if one was MISSED.
///
/// **The failure mode has no symptom.** A lane of `ir_verify` that rejects the
/// shape makes `ea_ir_bridge::ir_verify_reject` answer `true`, the method falls
/// back to the single-pass backend, and nothing anywhere fails — the optimizing
/// tier simply goes quiet on every method containing a guard, which is most of
/// them. No crash, no wrong answer, no red test: just a tier that stopped
/// doing anything, discoverable only by someone who went looking at a metric.
///
/// [`compile_optimizing`] asserts `used_ir_backend`, so this test converts that
/// silence into a failure. `idiv` is the subject because the four division arms
/// are one of the two producers wired to the gate (the other is the `String`
/// expansion), and a division carries the overflow guard unconditionally.
///
/// With the gate OFF this passes without exercising the token, which is the
/// honest reading: it is not a test OF the token, it is the sensor that makes
/// `CRATONVM_JIT_IR_GUARD_TOKEN=1` soakable. Run the suite with that variable
/// set and this is the test that reports a modelling gap. Measured on
/// 2026-09-17 with the gate on: this passes, and so does the rest of the jit
/// suite, so no consumer rejects the shape today.
#[test]
fn a_guarded_method_still_reaches_the_ir_tier_when_the_token_edge_is_emitted() {
    // static int f(int a, int b) { return a / b; }
    //  0: iload_0   1a
    //  1: iload_1   1b
    //  2: idiv      6c
    //  3: ireturn   ac
    let code = vec![0x1a, 0x1b, 0x6c, 0xac];
    let helpers = bounds_helpers();
    let body = compile_optimizing("guardedIdiv", "(II)I", code, 2, 2, &helpers);
    assert!(
        body.used_ir_backend,
        "a method carrying a division's overflow guard fell out of the IR tier; \
         with CRATONVM_JIT_IR_GUARD_TOKEN=1 that means a consumer rejects the \
         token edge and `ir_verify_reject` is silently sending every guarded \
         method to single-pass"
    );
}
