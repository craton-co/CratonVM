// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Round 15 wave 7, lane drainB: the single-pass FP_BITS arms for the `Float`
//! pair (IR2-2 of `jit-r15w6-irosr2-proposals-RETIRED-20261003.md`).
//!
//! `Float.floatToRawIntBits` / `Float.intBitsToFloat` lower to one `MOVD`
//! each. As for the `Double` pair (`intrinsic_fp_bits.rs`), the contract to
//! pin is that the move is RAW: every NaN payload, signalling and negative
//! ones included, survives, which is what separates `floatToRawIntBits` from
//! the canonicalising `floatToIntBits` the resolver does not match.

use cratonvm_jit::x64::compile;
use cratonvm_jit::{JitDirectCall, JitIntrinsic};
use cratonvm_jit_api::JitRuntimeHelpers;
use std::collections::{HashMap, HashSet};

/// Dummy runtime helpers (the same shape as `intrinsic_fp_bits.rs`): a bit
/// move never touches the heap, so the panicking stub is never reached.
fn dummy_helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("FP_BITS Float intrinsic test invoked an unwired runtime helper");
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

/// Zeroes, infinities, the canonical NaN, signalling and negative NaN
/// payloads, the extremes, and every single-bit pattern.
fn bit_patterns() -> Vec<u32> {
    let mut v: Vec<u32> = vec![
        0x0000_0000, // +0.0f
        0x8000_0000, // -0.0f
        0x3F80_0000, // 1.0f
        0xBF80_0000, // -1.0f
        0x7F80_0000, // +Inf
        0xFF80_0000, // -Inf
        0x7FC0_0000, // canonical quiet NaN
        0x7F80_0001, // signalling NaN, smallest payload
        0x7FBF_FFFF, // signalling NaN, largest payload
        0xFFC0_0001, // negative quiet NaN with payload
        0xFF80_0001, // negative signalling NaN
        0x0000_0001, // smallest subnormal
        0x7F7F_FFFF, // MAX_VALUE
        u32::MAX,
        i32::MAX as u32,
    ];
    for bit in 0..32 {
        v.push(1u32 << bit);
    }
    v
}

fn compile_one(code: Vec<u8>, intrinsic: JitIntrinsic, ret: u8) -> cratonvm_jit::CompiledMethod {
    compile(
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
                return_type: ret,
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
    .expect("JIT compilation failed")
}

/// `static int f(float a) { return Float.floatToRawIntBits(a); }`:
/// `fload_0` (0x22), `invokestatic` at pc 1, `ireturn` (0xac).
unsafe fn run_f2i(arg_bits: u32) -> u32 {
    let compiled = compile_one(
        vec![0x22, 0xb8, 0x00, 0x01, 0xac, 0, 0],
        JitIntrinsic::FloatToRawIntBits,
        b'I',
    );
    // SAFETY: machine code produced by the JIT from valid bytecode.
    let got = compiled
        .try_call(&[i64::from(arg_bits)])
        .expect("test JIT call");
    got as u32 // Cast: JIT ABI convention (an int result in the low 32 bits)
}

/// `static float f(int a) { return Float.intBitsToFloat(a); }`:
/// `iload_0` (0x1a), `invokestatic`, `freturn` (0xae).
unsafe fn run_i2f(arg_bits: u32) -> u32 {
    let compiled = compile_one(
        vec![0x1a, 0xb8, 0x00, 0x01, 0xae, 0, 0],
        JitIntrinsic::IntBitsToFloat,
        b'F',
    );
    // SAFETY: machine code produced by the JIT from valid bytecode.
    let got = compiled
        .try_call(&[i64::from(arg_bits as i32)])
        .expect("test JIT call");
    got as u32 // Cast: JIT ABI convention (an f32 bit pattern in the low 32 bits)
}

#[test]
fn float_to_raw_int_bits_is_the_identity_on_bits() {
    for &bits in &bit_patterns() {
        // SAFETY: see run_f2i.
        let got = unsafe { run_f2i(bits) };
        assert_eq!(
            got, bits,
            "floatToRawIntBits must hand back the argument's bits unchanged (bits={bits:#010x})"
        );
    }
}

#[test]
fn int_bits_to_float_is_the_identity_on_bits() {
    for &bits in &bit_patterns() {
        // SAFETY: see run_i2f.
        let got = unsafe { run_i2f(bits) };
        assert_eq!(
            got, bits,
            "intBitsToFloat must place the argument's bits in the FP result unchanged \
             (bits={bits:#010x})"
        );
    }
}

#[test]
fn the_float_pair_round_trips() {
    for &bits in &bit_patterns() {
        // SAFETY: see the two harnesses.
        let there = unsafe { run_i2f(bits) };
        let back = unsafe { run_f2i(there) };
        assert_eq!(
            back, bits,
            "a bits -> float -> bits round trip must be exact ({bits:#010x})"
        );
    }
}

/// The resolver claims the RAW pair and never the canonicalising
/// `floatToIntBits` (a `MOVD` does not canonicalise NaN).
#[cfg(target_arch = "x86_64")]
#[test]
fn the_resolver_claims_only_the_raw_float_pair() {
    assert!(
        cratonvm_jit::try_resolve_intrinsic("java/lang/Float", "floatToIntBits", "(F)I").is_none(),
        "floatToIntBits canonicalises NaN and must fall through to the native"
    );
    assert_eq!(
        cratonvm_jit::try_resolve_intrinsic("java/lang/Float", "floatToRawIntBits", "(F)I"),
        Some((JitIntrinsic::FloatToRawIntBits.as_entry(), 1, b'I'))
    );
    assert_eq!(
        cratonvm_jit::try_resolve_intrinsic("java/lang/Float", "intBitsToFloat", "(I)F"),
        Some((JitIntrinsic::IntBitsToFloat.as_entry(), 1, b'F'))
    );
}
