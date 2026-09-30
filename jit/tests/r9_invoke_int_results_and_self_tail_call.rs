// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 9, lane `invoke` — two call-site contracts the existing
//! differential suites cannot see.
//!
//! # 1. An `int` intrinsic result is SIGN-EXTENDED in its 64-bit slot
//!
//! The single-pass backend keeps every `int` value sign-extended to 64 bits
//! (`iadd`/`iushr`/`Math.min`/`rotateLeft` all end in `MOVSXD`), and the inline
//! mini-emitter treats `i2l` as a no-op on that promise. Seven `invokestatic`
//! intrinsics ended in a 32-bit instruction instead — `Math.abs(int)`,
//! `Integer.reverseBytes/highestOneBit/lowestOneBit/reverse/compare` and
//! `Long.compare` — so a negative result reached every 64-bit consumer as
//! `0x00000000_xxxxxxxx`. The existing `intrinsic_int_bits.rs` /
//! `intrinsic_long_bits.rs` suites cast the raw return to `i32` before
//! comparing, which is exactly the truncation that hides it. These tests keep
//! the raw 64-bit word: an empty `method_key` means `ireturn` narrows nothing,
//! so the word the intrinsic left in RAX is the word returned.
//!
//! # 2. A self-recursive TAIL call writes each argument to its PARAMETER home
//!
//! `static long f(long a, int b)` keeps `b` in JVM local 2, not 1. The tail
//! form used to write argument `i` to local `i`, so `b` landed in `a`'s dead
//! high half and the body kept reading the old `b`.

use cratonvm_jit::x64::{compile, compile_with_param_slots};
use cratonvm_jit::{JitDirectCall, JitIntrinsic};
use cratonvm_jit_api::JitRuntimeHelpers;
use std::collections::{HashMap, HashSet};

/// Stub helpers, as `intrinsic_long_bits.rs` builds them: nothing here
/// touches the heap, fields, type checks or dispatch.
fn dummy_helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("r9 invoke test invoked an unwired runtime helper");
    }
    let s = stub as *const () as usize;
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
        set_throw_bci: throw_bci,
        service_callee_deopt: deopt_unserviceable,
        ..Default::default()
    }
}

/// Compile `static int f(int x) { return <intrinsic>(x); }` and return the RAW
/// 64-bit word the compiled code returns for `x`.
///
/// `iload_0; invokestatic #1; ireturn` — the invoke sits at pc 1.
unsafe fn raw_unary_int(intrinsic: JitIntrinsic, x: i32) -> i64 {
    let code: Vec<u8> = vec![0x1a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let compiled = compile(
        &code,
        5,
        1,
        1,
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
                return_type: b'I',
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
        None,
    )
    .expect("JIT compilation of the unary int intrinsic wrapper failed");
    compiled.try_call(&[x as i64]).expect("test JIT call")
}

/// Compile `static int f(<T> x, <T> y) { return <intrinsic>(x, y); }` for an
/// `int` (`iload_0; iload_1`) or `long` (`lload_0; lload_1`) pair, and return
/// the RAW 64-bit word.
unsafe fn raw_binary(intrinsic: JitIntrinsic, long_args: bool, x: i64, y: i64) -> i64 {
    let (l0, l1) = if long_args {
        (0x1e, 0x1f)
    } else {
        (0x1a, 0x1b)
    };
    let code: Vec<u8> = vec![l0, l1, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    let compiled = compile(
        &code,
        6,
        2,
        2,
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
                return_type: b'I',
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
        None,
    )
    .expect("JIT compilation of the binary intrinsic wrapper failed");
    compiled.try_call(&[x, y]).expect("test JIT call")
}

fn int_edges() -> Vec<i32> {
    let mut v = vec![0, 1, -1, 2, -2, 0x80, 0xFF, i32::MIN, i32::MAX, 0x0102_0304];
    for s in 0..32 {
        v.push(1i32 << s);
    }
    v
}

#[test]
fn unary_int_intrinsics_return_a_sign_extended_word() {
    let cases: [(JitIntrinsic, &str, fn(i32) -> i32); 5] = [
        (JitIntrinsic::MathAbsInt, "Math.abs", |x| x.wrapping_abs()),
        (JitIntrinsic::IntReverseBytes, "Integer.reverseBytes", |x| {
            x.swap_bytes()
        }),
        (
            JitIntrinsic::IntHighestOneBit,
            "Integer.highestOneBit",
            |x| {
                if x == 0 {
                    0
                } else {
                    ((1u32 << 31) >> (x as u32).leading_zeros()) as i32
                }
            },
        ),
        (JitIntrinsic::IntLowestOneBit, "Integer.lowestOneBit", |x| {
            x & x.wrapping_neg()
        }),
        (JitIntrinsic::IntReverse, "Integer.reverse", |x| {
            x.reverse_bits()
        }),
    ];
    for (intrinsic, name, reference) in cases {
        for x in int_edges() {
            // SAFETY: machine code produced by the JIT from valid bytecode.
            let raw = unsafe { raw_unary_int(intrinsic, x) };
            let want = reference(x) as i64; // sign-extended, as an int slot must be
            assert_eq!(
                raw, want,
                "{name}({x:#x}) left {raw:#018x} in its 64-bit slot; an int result must be \
                 sign-extended ({want:#018x})"
            );
        }
    }
}

#[test]
fn integer_compare_returns_a_sign_extended_minus_one() {
    for x in int_edges() {
        for y in int_edges() {
            // SAFETY: machine code produced by the JIT from valid bytecode.
            let raw = unsafe { raw_binary(JitIntrinsic::IntCompare, false, x as i64, y as i64) };
            let want = x.cmp(&y) as i64;
            assert_eq!(raw, want, "Integer.compare({x}, {y}) raw word {raw:#018x}");
        }
    }
}

#[test]
fn long_compare_returns_a_sign_extended_minus_one() {
    let vals = [
        0i64,
        1,
        -1,
        i64::MIN,
        i64::MAX,
        0x1_0000_0000,
        -0x1_0000_0000,
    ];
    for &x in &vals {
        for &y in &vals {
            // SAFETY: machine code produced by the JIT from valid bytecode.
            let raw = unsafe { raw_binary(JitIntrinsic::LongCompare, true, x, y) };
            let want = x.cmp(&y) as i64;
            assert_eq!(raw, want, "Long.compare({x}, {y}) raw word {raw:#018x}");
        }
    }
}

/// `static long f(long a, int b) { if (a >= 10) return a + b; return f(a + 1, b + 100); }`
///
/// ```text
///  0: lload_0
///  1: bipush 10
///  3: i2l
///  4: lcmp
///  5: iflt +8        -> 13
///  8: lload_0
///  9: iload_2
/// 10: i2l
/// 11: ladd
/// 12: lreturn
/// 13: lload_0
/// 14: lconst_1
/// 15: ladd
/// 16: iload_2
/// 17: bipush 100
/// 19: iadd
/// 20: invokestatic f   (self; no invoke_info, no direct call)
/// 23: lreturn          (tail position)
/// ```
///
/// `f(0, 0)` recurses ten times and answers `10 + 1000 = 1010`. With `b`
/// written to local 1 (the old identity mapping) instead of local 2, `b` never
/// changes and the answer is `10`.
#[test]
fn a_self_tail_call_writes_each_argument_to_its_parameter_slot() {
    let code: Vec<u8> = vec![
        0x1e, 0x10, 0x0a, 0x85, 0x94, 0x9b, 0x00, 0x08, 0x1e, 0x1c, 0x85, 0x61, 0xad, 0x1e, 0x0a,
        0x61, 0x1c, 0x10, 0x64, 0x60, 0xb8, 0x00, 0x01, 0xad, 0, 0,
    ];
    let code_len = 24;
    let helpers = dummy_helpers();
    let compiled = compile_with_param_slots(
        // Not a door: hand-built bytecode with no method identity to admit.
        &cratonvm_jit::compile_gate::CompileAdmission::for_backend_test(),
        &code,
        code_len,
        2, // num_params: a (long), b (int)
        3, // max_locals: a spans 0-1, b is 2
        false,
        Vec::new(),         // multianewarray_info
        Vec::new(),         // field_info
        Vec::new(),         // typecheck_info
        Vec::new(),         // static_field_info
        Vec::new(),         // new_info
        Vec::new(),         // new_deferred_info
        Vec::new(),         // anewarray_info
        Vec::new(),         // anewarray_deferred_info
        Vec::new(),         // invoke_info: none, so pc 20 is the self-recursive arm
        Vec::new(),         // direct_calls
        Vec::new(),         // mic_slots
        Vec::new(),         // pic_slots
        Vec::new(),         // ldc_info
        Vec::new(),         // ldc_string_info
        Vec::new(),         // ldc_class_info
        Vec::new(),         // ldc2w_info
        Default::default(), // ldc_fp_pcs
        HashMap::new(),
        HashMap::new(),
        &helpers,
        HashSet::new(),
        HashMap::new(),
        HashMap::new(), // inline_guard_variants
        None,           // string_layout
        &[0, 2],        // param_jvm_slots
        3,              // param_slot_span
        0,              // param_oop_mask
        Vec::new(),     // compact_field_info
        "R9.f:(JI)J",
        None,       // despec
        Vec::new(), // indy_info
        None,       // elidable_init_pcs
        cratonvm_jit::x64::BackendRequest::default(),
    )
    .expect("JIT compilation of the self-tail-call method failed");
    // SAFETY: machine code produced by the JIT from valid bytecode.
    let got = unsafe { compiled.try_call(&[0, 0]).expect("test JIT call") };
    assert_eq!(
        got, 1010,
        "the tail call must deposit `b` in JVM local 2, where the body reads it"
    );
}
