// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Differential tests for the FP_BITS JIT intrinsic family.
//!
//! `Double.doubleToRawLongBits` and `Double.longBitsToDouble` lower to one
//! `MOVQ` each. The contract that needs pinning is not the arithmetic -- there
//! is none -- but that the move is RAW: every one of the 2^52 NaN payloads
//! must survive a round trip, which is exactly what distinguishes
//! `doubleToRawLongBits` from the canonicalising `doubleToLongBits` the
//! resolver deliberately does not match.
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
        panic!("FP_BITS intrinsic test invoked an unwired runtime helper");
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

/// Bit patterns worth a round trip: zeroes and infinities, the canonical NaN,
/// and a spread of NaN payloads INCLUDING the negative-signed ones
/// (`0xFFFC_…` and up) that `nan-payloads-lost-to-the-compactvalue-tag-
/// collision-FIXED-20260828` records the value encoding having destroyed
/// elsewhere until 2026-08-28. If the lowering ever stops being a raw move,
/// those are the rows that catch it.
fn bit_patterns() -> Vec<i64> {
    let mut v: Vec<i64> = vec![
        0x0000_0000_0000_0000u64 as i64, // +0.0
        0x8000_0000_0000_0000u64 as i64, // -0.0
        0x3FF0_0000_0000_0000u64 as i64, // 1.0
        0xBFF0_0000_0000_0000u64 as i64, // -1.0
        0x7FF0_0000_0000_0000u64 as i64, // +Inf
        0xFFF0_0000_0000_0000u64 as i64, // -Inf
        0x7FF8_0000_0000_0000u64 as i64, // canonical quiet NaN
        0x7FF0_0000_0000_0001u64 as i64, // signalling NaN, smallest payload
        0xFFF8_0000_0000_0001u64 as i64, // negative quiet NaN with payload
        0xFFFC_541A_8000_0000u64 as i64, // the widening the census page names
        0xFFFE_5E0E_8000_0000u64 as i64, // and its sibling
        0x0000_0000_0000_0001u64 as i64, // smallest subnormal
        0x7FEF_FFFF_FFFF_FFFFu64 as i64, // MAX_VALUE
        i64::MIN,
        i64::MAX,
        -1,
        1,
    ];
    for bit in 0..64 {
        v.push(1i64 << bit);
    }
    v
}

/// Compile `static long f(double a) { return Double.doubleToRawLongBits(a); }`
/// and return the raw i64 the JIT produced.
///
/// Bytecode: `dload_0` (0x26), `invokestatic` at pc 1, `lreturn` (0xad). The
/// JIT models a `double` argument as a single local slot / single i64 register
/// arg, exactly as it models a `long`.
unsafe fn run_d2l(arg_bits: i64) -> i64 {
    let code: Vec<u8> = vec![0x26, 0xb8, 0x00, 0x01, 0xad, 0, 0];
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
                entry: JitIntrinsic::DoubleToRawLongBits.as_entry(),
                needs_context: false,
                num_params: 1,
                return_type: b'J',
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
    .expect("JIT compilation failed");
    // SAFETY: machine code produced by the JIT from valid bytecode.
    compiled.try_call(&[arg_bits]).expect("test JIT call")
}

/// The mirror: `static double f(long a) { return Double.longBitsToDouble(a); }`.
///
/// `lload_0` (0x1e), `invokestatic`, `dreturn` (0xaf).
unsafe fn run_l2d(arg_bits: i64) -> i64 {
    let code: Vec<u8> = vec![0x1e, 0xb8, 0x00, 0x01, 0xaf, 0, 0];
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
                entry: JitIntrinsic::LongBitsToDouble.as_entry(),
                needs_context: false,
                num_params: 1,
                return_type: b'D',
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
    .expect("JIT compilation failed");
    // SAFETY: machine code produced by the JIT from valid bytecode.
    compiled.try_call(&[arg_bits]).expect("test JIT call")
}

#[test]
fn double_to_raw_long_bits_is_the_identity_on_bits() {
    for &bits in &bit_patterns() {
        // SAFETY: see run_d2l.
        let got = unsafe { run_d2l(bits) };
        assert_eq!(
            got, bits,
            "doubleToRawLongBits must hand back the argument's bits unchanged \
             (bits={bits:#018x}); a canonicalising lowering fails here on the \
             NaN rows"
        );
    }
}

#[test]
fn long_bits_to_double_is_the_identity_on_bits() {
    for &bits in &bit_patterns() {
        // SAFETY: see run_l2d.
        let got = unsafe { run_l2d(bits) };
        assert_eq!(
            got, bits,
            "longBitsToDouble must place the argument's bits in the FP result \
             unchanged (bits={bits:#018x})"
        );
    }
}

#[test]
fn the_two_intrinsics_round_trip_each_other() {
    for &bits in &bit_patterns() {
        // SAFETY: see the two harnesses.
        let there = unsafe { run_l2d(bits) };
        let back = unsafe { run_d2l(there) };
        assert_eq!(
            back, bits,
            "a bits -> double -> bits round trip must be exact (bits={bits:#018x})"
        );
    }
}

/// The resolver must NOT claim `doubleToLongBits`, which canonicalises every
/// NaN to `0x7ff8000000000000`. A `MOVQ` does not implement that, so admitting
/// it would silently change the value of every signalling NaN.
///
/// Verified by BREAKING it: adding `("doubleToLongBits", "(D)J")` to the
/// FP_BITS region of `try_resolve_intrinsic` fails this test.
#[test]
fn the_canonicalising_sibling_is_not_an_intrinsic() {
    assert!(
        cratonvm_jit::try_resolve_intrinsic("java/lang/Double", "doubleToLongBits", "(D)J")
            .is_none(),
        "doubleToLongBits canonicalises NaN and has no single-instruction \
         lowering; it must fall through to the native"
    );
    // The two that ARE claimed, so this test cannot pass by the resolver
    // having been switched off entirely.
    assert!(
        cratonvm_jit::try_resolve_intrinsic("java/lang/Double", "doubleToRawLongBits", "(D)J")
            .is_some(),
        "the RAW conversion must be claimed"
    );
    assert!(
        cratonvm_jit::try_resolve_intrinsic("java/lang/Double", "longBitsToDouble", "(J)D")
            .is_some(),
        "and so must its mirror"
    );
}
