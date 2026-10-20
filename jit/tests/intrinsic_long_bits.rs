// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Differential tests for the LONG_BITS JIT intrinsic family (Phase 1b).
//!
//! Each test JIT-compiles a tiny synthetic method whose only body is an
//! `invokestatic` of a `java.lang.Long` bit-op, wired to the corresponding
//! `JitIntrinsic` sentinel via a `JitDirectCall`. The JIT result is then
//! asserted bit-identical to the Rust reference (`u64`/`i64` primitives,
//! which implement exactly the JDK semantics) over a matrix of edge values:
//! 0, -1, 1, i64::MIN, i64::MAX, and powers of two.
//!
//! Harness modelled on `jit/tests/differential.rs`.

use cratonvm_jit::x64::compile;
use cratonvm_jit::{JitDirectCall, JitIntrinsic};
use cratonvm_jit_api::JitRuntimeHelpers;
use std::collections::{HashMap, HashSet};

/// Dummy runtime helpers — bit-op intrinsics never touch the heap, fields,
/// type checks, or dispatch, so a panicking stub pointer is never reached.
fn dummy_helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("LONG_BITS intrinsic test invoked an unwired runtime helper");
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

/// Edge-value matrix exercised by every test.
fn long_edge_values() -> Vec<i64> {
    let mut v = vec![
        0i64,
        -1,
        1,
        i64::MIN,
        i64::MAX,
        2,
        3,
        0x00FF_00FF_00FF_00FFu64 as i64,
        0x0123_4567_89AB_CDEFu64 as i64,
        -2,
        0x8000_0000_0000_0000u64 as i64,
    ];
    // Every power of two and its negation (wrapping: 1<<63 negates to itself).
    for bit in 0..64u32 {
        v.push(1i64 << bit);
        v.push((1i64 << bit).wrapping_neg());
    }
    v
}

/// Compile `static long/int f(long a) { return Long.OP(a); }` and return the
/// raw i64 the JIT produced when called with `arg`.
///
/// Bytecode: `lload_0` (0x1e), `invokestatic` (0xb8 idx) at pc 1,
/// `lreturn`/`ireturn` at pc 4. The JIT models a `long` argument as a single
/// local slot / single i64 register arg (see `test_compile_long_add`), so
/// `num_params` and `max_locals` are both 1.
unsafe fn run_unary(intrinsic: JitIntrinsic, ret_type: u8, arg: i64) -> i64 {
    let ret_op: u8 = if ret_type == b'J' { 0xad } else { 0xac }; // lreturn / ireturn
    let code: Vec<u8> = vec![0x1e, 0xb8, 0x00, 0x01, ret_op, 0, 0];
    let code_len = 5;
    let compiled = compile(
        &code,
        code_len,
        1, // num_params: one long argument
        1, // max_locals
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![(
            1,
            JitDirectCall {
                entry: intrinsic.as_entry(),
                needs_context: false,
                num_params: 1,
                return_type: ret_type,
                guard_class_id: 0,
            },
        )],
        Vec::new(),
        Vec::new(),
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
    // SAFETY: machine code produced by the JIT from valid bytecode.
    compiled.try_call(&[arg]).expect("test JIT call")
}

/// Compile `static int f(long x, long y) { return Long.OP(x, y); }`.
///
/// Bytecode: `lload_0` (0x1e), `lload_1` (0x1f), `invokestatic` at pc 2,
/// `ireturn`. The JIT models each `long` argument as a single local slot /
/// single i64 register arg (see `test_compile_long_add`), so `num_params`
/// and `max_locals` are both 2.
unsafe fn run_binary(intrinsic: JitIntrinsic, ret_type: u8, x: i64, y: i64) -> i64 {
    let ret_op: u8 = if ret_type == b'J' { 0xad } else { 0xac };
    let code: Vec<u8> = vec![0x1e, 0x1f, 0xb8, 0x00, 0x01, ret_op, 0, 0];
    let code_len = 6;
    let compiled = compile(
        &code,
        code_len,
        2, // num_params: two long arguments
        2, // max_locals
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![(
            2,
            JitDirectCall {
                entry: intrinsic.as_entry(),
                needs_context: false,
                num_params: 2,
                return_type: ret_type,
                guard_class_id: 0,
            },
        )],
        Vec::new(),
        Vec::new(),
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
    // SAFETY: machine code produced by the JIT from valid bytecode.
    compiled.try_call(&[x, y]).expect("test JIT call")
}

#[test]
fn long_bit_count_matches_reference() {
    if !cratonvm_jit::x64::has_popcnt() {
        eprintln!("skipping long_bit_count: host lacks POPCNT");
        return;
    }
    for &a in &long_edge_values() {
        let want = (a as u64).count_ones() as i64;
        // SAFETY: see run_unary.
        let got = unsafe { run_unary(JitIntrinsic::LongBitCount, b'I', a) };
        assert_eq!(got, want, "Long.bitCount({a:#018x})");
    }
}

#[test]
fn long_number_of_leading_zeros_matches_reference() {
    for &a in &long_edge_values() {
        let want = (a as u64).leading_zeros() as i64;
        // SAFETY: see run_unary.
        let got = unsafe { run_unary(JitIntrinsic::LongNumberOfLeadingZeros, b'I', a) };
        assert_eq!(got, want, "Long.numberOfLeadingZeros({a:#018x})");
    }
}

#[test]
fn long_number_of_trailing_zeros_matches_reference() {
    for &a in &long_edge_values() {
        let want = (a as u64).trailing_zeros() as i64;
        // SAFETY: see run_unary.
        let got = unsafe { run_unary(JitIntrinsic::LongNumberOfTrailingZeros, b'I', a) };
        assert_eq!(got, want, "Long.numberOfTrailingZeros({a:#018x})");
    }
}

#[test]
fn long_reverse_bytes_matches_reference() {
    for &a in &long_edge_values() {
        let want = (a as u64).swap_bytes() as i64;
        // SAFETY: see run_unary.
        let got = unsafe { run_unary(JitIntrinsic::LongReverseBytes, b'J', a) };
        assert_eq!(got, want, "Long.reverseBytes({a:#018x})");
    }
}

#[test]
fn long_highest_one_bit_matches_reference() {
    for &a in &long_edge_values() {
        // JDK Long.highestOneBit: value with only the MSB of `a` set, 0 if a==0.
        let u = a as u64;
        let want = if u == 0 {
            0i64
        } else {
            (1u64 << (63 - u.leading_zeros())) as i64
        };
        // SAFETY: see run_unary.
        let got = unsafe { run_unary(JitIntrinsic::LongHighestOneBit, b'J', a) };
        assert_eq!(got, want, "Long.highestOneBit({a:#018x})");
    }
}

#[test]
fn long_lowest_one_bit_matches_reference() {
    for &a in &long_edge_values() {
        // JDK Long.lowestOneBit: a & -a.
        let want = (a as u64 & (a as u64).wrapping_neg()) as i64;
        // SAFETY: see run_unary.
        let got = unsafe { run_unary(JitIntrinsic::LongLowestOneBit, b'J', a) };
        assert_eq!(got, want, "Long.lowestOneBit({a:#018x})");
    }
}

#[test]
fn long_compare_matches_reference() {
    let vals = long_edge_values();
    for &x in &vals {
        for &y in &vals {
            let want = match x.cmp(&y) {
                std::cmp::Ordering::Less => -1i64,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            };
            // SAFETY: see run_binary. Long.compare returns an int; truncate
            // the raw i64 to i32 before widening so a -1 result compares
            // equal regardless of how the JIT extends the int return value.
            let got = unsafe { run_binary(JitIntrinsic::LongCompare, b'I', x, y) as i32 as i64 };
            assert_eq!(got, want, "Long.compare({x:#018x}, {y:#018x})");
        }
    }
}

/// Distances for the 64-bit rotate intrinsics: 0, sub-width, exactly width,
/// > width (x86 masks CL & 0x3f), and negative. Passed as the int distance.
fn long_rotate_distances() -> Vec<i32> {
    vec![
        0,
        1,
        7,
        31,
        32,
        33,
        63,
        64,
        65,
        127,
        128,
        -1,
        -7,
        -64,
        -65,
        i32::MIN,
        i32::MAX,
    ]
}

#[test]
fn long_rotate_left_matches_reference() {
    // Verify the (JI)J matcher registers this entry too.
    assert!(
        cratonvm_jit::try_resolve_intrinsic("java/lang/Long", "rotateLeft", "(JI)J").is_some(),
        "Long.rotateLeft must register"
    );
    for &v in &long_edge_values() {
        for &d in &long_rotate_distances() {
            // i64::rotate_left masks the distance mod 64, matching
            // Long.rotateLeft and x86 ROL r64,CL (CL & 0x3f).
            let reference = v.rotate_left((d as u32) & 63);
            // SAFETY: see run_binary. Distance rides in the low byte (→ CL).
            let got = unsafe { run_binary(JitIntrinsic::LongRotateLeft, b'J', v, d as i64) };
            assert_eq!(got, reference, "Long.rotateLeft({v:#018x}, {d})");
        }
    }
}

#[test]
fn long_rotate_right_matches_reference() {
    assert!(
        cratonvm_jit::try_resolve_intrinsic("java/lang/Long", "rotateRight", "(JI)J").is_some(),
        "Long.rotateRight must register"
    );
    for &v in &long_edge_values() {
        for &d in &long_rotate_distances() {
            let reference = v.rotate_right((d as u32) & 63);
            // SAFETY: see run_binary.
            let got = unsafe { run_binary(JitIntrinsic::LongRotateRight, b'J', v, d as i64) };
            assert_eq!(got, reference, "Long.rotateRight({v:#018x}, {d})");
        }
    }
}
