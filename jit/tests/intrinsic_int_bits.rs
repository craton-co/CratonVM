// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Differential tests for the INT_BITS JIT intrinsic family — the
//! `java.lang.Integer` bit-manipulation methods (Phase 1a).
//!
//! Each test JIT-compiles a tiny synthetic method whose only operation is an
//! `invokestatic` of one `Integer` intrinsic, then asserts the JITted result
//! is bit-identical to the Rust reference implementation (`u32::count_ones`,
//! `i32::leading_zeros`, …) over a matrix of edge values: 0, -1, 1,
//! `i32::MIN`, `i32::MAX`, and every power of two.
//!
//! The harness mirrors `differential.rs`: a stub `JitRuntimeHelpers` is
//! sufficient because bit ops are pure leaves with no heap or call sites.

use cratonvm_jit::x64::compile;
use cratonvm_jit::JitDirectCall;
use cratonvm_jit_api::JitRuntimeHelpers;
use std::collections::{HashMap, HashSet};

/// Stub runtime helpers — no INT_BITS intrinsic touches the heap, fields,
/// type checks or dispatch, so the stub pointer is never invoked.
fn stub_helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("INT_BITS intrinsic test invoked an unwired runtime helper");
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

/// Edge-value matrix exercised by every single-argument intrinsic.
fn edge_values() -> Vec<i32> {
    let mut v = vec![
        0i32,
        -1,
        1,
        2,
        3,
        7,
        0x55,
        -0x55,
        i32::MIN,
        i32::MAX,
        0x0F0F_0F0F_u32 as i32,
    ];
    for shift in 0..32 {
        v.push(1i32 << shift);
    }
    v
}

/// JIT-compile `int f(int x) { return Integer.<intr>(x); }` and return a
/// closure that runs it. Bytecode:
///   iload_0 (0x1a), invokestatic (0xb8 0x00 0x01), ireturn (0xac)
/// The `invokestatic` opcode sits at pc 1 — that is the `direct_calls` key.
fn compile_unary(entry: usize) -> impl Fn(i32) -> i32 {
    let code: Vec<u8> = vec![0x1a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let compiled = compile(
        &code,
        5,
        1, // num_params
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
                entry,
                needs_context: false,
                num_params: 1,
                return_type: b'I',
                guard_class_id: 0,
            },
        )],
        Vec::new(), // mic_slots
        Vec::new(), // pic_slots
        Vec::new(), // ldc_info
        Vec::new(), // ldc2w_info
        HashMap::new(),
        HashMap::new(),
        &stub_helpers(),
        HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("JIT compilation of unary INT_BITS intrinsic failed");
    move |x: i32| {
        // SAFETY: `compiled` was produced by the JIT from valid bytecode and
        // the mmap region is executable.
        unsafe { compiled.try_call(&[x as i64]).expect("test JIT call") as i32 }
    }
}

/// JIT-compile `int f(int x, int y) { return Integer.<intr>(x, y); }`.
/// Bytecode: iload_0, iload_1, invokestatic, ireturn — the invokestatic is
/// at pc 2.
fn compile_binary(entry: usize) -> impl Fn(i32, i32) -> i32 {
    let code: Vec<u8> = vec![0x1a, 0x1b, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let compiled = compile(
        &code,
        6,
        2, // num_params
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
                entry,
                needs_context: false,
                num_params: 2,
                return_type: b'I',
                guard_class_id: 0,
            },
        )],
        Vec::new(), // mic_slots
        Vec::new(), // pic_slots
        Vec::new(), // ldc_info
        Vec::new(), // ldc2w_info
        HashMap::new(),
        HashMap::new(),
        &stub_helpers(),
        HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("JIT compilation of binary INT_BITS intrinsic failed");
    move |x: i32, y: i32| {
        // SAFETY: see `compile_unary`.
        unsafe {
            compiled
                .try_call(&[x as i64, y as i64])
                .expect("test JIT call") as i32
        }
    }
}

/// Resolve an `Integer` method to its intrinsic entry sentinel, or skip the
/// test when the host CPU lacks the required feature (the matcher returns
/// `None` and the call would otherwise fall back to native dispatch).
fn resolve(name: &str, descriptor: &str) -> Option<usize> {
    cratonvm_jit::try_resolve_intrinsic("java/lang/Integer", name, descriptor)
        .map(|(entry, _, _)| entry)
}

#[test]
fn int_bit_count_matches_reference() {
    let entry = match resolve("bitCount", "(I)I") {
        Some(e) => e,
        None => {
            eprintln!("skipping bitCount: host lacks POPCNT");
            return;
        }
    };
    let f = compile_unary(entry);
    for x in edge_values() {
        assert_eq!(
            f(x),
            (x as u32).count_ones() as i32,
            "bitCount({x}) mismatch"
        );
    }
}

#[test]
fn int_number_of_leading_zeros_matches_reference() {
    // Always registered: LZCNT path or BSR fallback.
    let entry = resolve("numberOfLeadingZeros", "(I)I").expect("nlz must register");
    let f = compile_unary(entry);
    for x in edge_values() {
        assert_eq!(
            f(x),
            (x as u32).leading_zeros() as i32,
            "numberOfLeadingZeros({x}) mismatch"
        );
    }
}

#[test]
fn int_number_of_trailing_zeros_matches_reference() {
    let entry = resolve("numberOfTrailingZeros", "(I)I").expect("ntz must register");
    let f = compile_unary(entry);
    for x in edge_values() {
        assert_eq!(
            f(x),
            (x as u32).trailing_zeros() as i32,
            "numberOfTrailingZeros({x}) mismatch"
        );
    }
}

#[test]
fn int_reverse_bytes_matches_reference() {
    let entry = resolve("reverseBytes", "(I)I").expect("reverseBytes must register");
    let f = compile_unary(entry);
    for x in edge_values() {
        assert_eq!(
            f(x),
            (x as u32).swap_bytes() as i32,
            "reverseBytes({x}) mismatch"
        );
    }
}

#[test]
fn int_highest_one_bit_matches_reference() {
    let entry = resolve("highestOneBit", "(I)I").expect("highestOneBit must register");
    let f = compile_unary(entry);
    for x in edge_values() {
        // JDK: i & (MIN_VALUE >>> numberOfLeadingZeros(i)); 0 for i == 0.
        let reference = if x == 0 {
            0
        } else {
            1i32 << (31 - (x as u32).leading_zeros())
        };
        assert_eq!(f(x), reference, "highestOneBit({x}) mismatch");
    }
}

#[test]
fn int_lowest_one_bit_matches_reference() {
    let entry = resolve("lowestOneBit", "(I)I").expect("lowestOneBit must register");
    let f = compile_unary(entry);
    for x in edge_values() {
        // JDK: i & -i.
        assert_eq!(
            f(x),
            (x as i32).wrapping_neg() & x,
            "lowestOneBit({x}) mismatch"
        );
    }
}

#[test]
fn int_reverse_matches_reference() {
    let entry = resolve("reverse", "(I)I").expect("reverse must register");
    let f = compile_unary(entry);
    for x in edge_values() {
        assert_eq!(
            f(x),
            (x as u32).reverse_bits() as i32,
            "reverse({x}) mismatch"
        );
    }
}

#[test]
fn int_compare_matches_reference() {
    let entry = resolve("compare", "(II)I").expect("compare must register");
    let f = compile_binary(entry);
    let vals = edge_values();
    for &x in &vals {
        for &y in &vals {
            // JDK: (x < y) ? -1 : ((x == y) ? 0 : 1).
            let reference = x.cmp(&y) as i32;
            assert_eq!(f(x, y), reference, "compare({x}, {y}) mismatch");
        }
    }
}

/// Distances exercised by the rotate intrinsics: 0, sub-width, exactly the
/// width, > width (x86 masks CL & 0x1f for 32-bit), and negative (the JDK
/// rotates by `distance mod 32`, which x86's CL masking reproduces).
fn rotate_distances() -> Vec<i32> {
    vec![
        0,
        1,
        7,
        8,
        15,
        16,
        31,
        32,
        33,
        63,
        64,
        65,
        -1,
        -7,
        -32,
        -33,
        i32::MIN,
        i32::MAX,
    ]
}

#[test]
fn int_rotate_left_matches_reference() {
    let entry = resolve("rotateLeft", "(II)I").expect("rotateLeft must register");
    let f = compile_binary(entry);
    for &v in &edge_values() {
        for &d in &rotate_distances() {
            // i32::rotate_left masks the distance mod 32, exactly matching
            // Integer.rotateLeft and x86 ROL r32,CL (CL & 0x1f).
            let reference = v.rotate_left((d as u32) & 31);
            assert_eq!(f(v, d), reference, "rotateLeft({v}, {d}) mismatch");
        }
    }
}

#[test]
fn int_rotate_right_matches_reference() {
    let entry = resolve("rotateRight", "(II)I").expect("rotateRight must register");
    let f = compile_binary(entry);
    for &v in &edge_values() {
        for &d in &rotate_distances() {
            let reference = v.rotate_right((d as u32) & 31);
            assert_eq!(f(v, d), reference, "rotateRight({v}, {d}) mismatch");
        }
    }
}
