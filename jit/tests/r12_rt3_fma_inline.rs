// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Round 12 wave 8, lane rt3: the single-pass `Math.fma` / `StrictMath.fma`
//! arms (`jit/src/x64/op_invoke.rs`, `MATH_FMA_{DOUBLE,FLOAT}_INTRINSIC`)
//! emit `VFMADD213SD` / `VFMADD213SS` inline when the host has FMA3, and call
//! the `math_fma_*` helper otherwise (or under `CRATONVM_JIT_FMA_INLINE=0`).
//!
//! Executed, not inspected: `double f() { return Math.fma(A, B, C); }` (the
//! operands are `ldc2_w` / `ldc` constants) is compiled by the single-pass
//! backend and run. The helper slots hold counting stubs, so each case also
//! proves which form ran: on an FMA3 host with the switch on the helper must
//! never be reached. The operand triples are the page's two double-rounding
//! traps (`r12w2-hunter-fma-helpers-trust-the-c-runtime-fma-20260926.md`),
//! cases an unfused `a*b + c` gets wrong, and the operand ORDER (`fma(a, b,
//! c)` is not `fma(c, b, a)`), each against a hand-derived bit pattern.
//!
//! Harness: `intrinsic_int_bits.rs`'s stub helper table.

use cratonvm_jit::x64::compile;
use cratonvm_jit::JitDirectCall;
use cratonvm_jit_api::JitRuntimeHelpers;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};

thread_local! {
    static HELPER_CALLS: Cell<u32> = const { Cell::new(0) };
}

/// The fallback helper stand-ins. They count, and answer with an unfused
/// `mul_add` (whatever the C runtime does), which is only compared on hosts
/// that take the helper path.
extern "C" fn fma_double_helper(a: f64, b: f64, c: f64) -> f64 {
    HELPER_CALLS.with(|n| n.set(n.get() + 1));
    a.mul_add(b, c)
}

extern "C" fn fma_float_helper(a: f32, b: f32, c: f32) -> f32 {
    HELPER_CALLS.with(|n| n.set(n.get() + 1));
    a.mul_add(b, c)
}

fn stub_helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("r12_rt3_fma_inline invoked an unwired runtime helper");
    }
    let s = stub as *const () as usize;
    unsafe extern "C" fn record_throw_bci(_bci: i64) {}
    unsafe extern "C" fn deopt_unserviceable_stub(
        _vm: i64,
        _info: i64,
        _args: i64,
        _n: i64,
    ) -> i64 {
        i64::MIN
    }
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
        math_fma_double: fma_double_helper as *const () as usize,
        math_fma_float: fma_float_helper as *const () as usize,
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
        set_throw_bci: record_throw_bci as *const () as usize,
        service_callee_deopt: deopt_unserviceable_stub as *const () as usize,
        ..Default::default()
    }
}

/// Whether the arm is expected to emit the instruction: the host has FMA3
/// with the OS's AVX state (the same std detection `cpu_features::has_fma`
/// reads) and the kill switch is not off.
fn expect_inline() -> bool {
    #[cfg(target_arch = "x86_64")]
    let hw = is_x86_feature_detected!("fma") && is_x86_feature_detected!("avx");
    #[cfg(not(target_arch = "x86_64"))]
    let hw = false;
    let off = std::env::var("CRATONVM_JIT_FMA_INLINE").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    });
    hw && !off
}

fn entry(descriptor: &str) -> usize {
    cratonvm_jit::try_resolve_intrinsic("java/lang/Math", "fma", descriptor)
        .map(|(entry, _, _)| entry)
        .expect("Math.fma is always an intrinsic")
}

/// `double f() { return Math.fma(A, B, C); }`:
/// `ldc2_w #1; ldc2_w #2; ldc2_w #3; invokestatic #4; dreturn`.
/// Returns the result and the number of helper calls the run made.
fn run_double(a: f64, b: f64, c: f64) -> (f64, u32) {
    let code: Vec<u8> = vec![
        0x14, 0x00, 0x01, // 0: ldc2_w A
        0x14, 0x00, 0x02, // 3: ldc2_w B
        0x14, 0x00, 0x03, // 6: ldc2_w C
        0xb8, 0x00, 0x04, // 9: invokestatic Math.fma(DDD)D
        0xaf, // 12: dreturn
        0, 0,
    ];
    let compiled = compile(
        &code,
        13,
        0,
        0,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![(
            9,
            JitDirectCall {
                entry: entry("(DDD)D"),
                needs_context: false,
                num_params: 3,
                return_type: b'D',
                guard_class_id: 0,
            },
        )],
        Vec::new(), // mic_slots
        Vec::new(), // pic_slots
        Vec::new(), // ldc_info
        vec![
            (0, a.to_bits() as i64),
            (3, b.to_bits() as i64),
            (6, c.to_bits() as i64),
        ],
        HashMap::new(),
        HashMap::new(),
        &stub_helpers(),
        HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("single-pass compile of Math.fma(DDD)D");
    let before = HELPER_CALLS.with(Cell::get);
    // SAFETY: the method was compiled from valid bytecode into an executable
    // buffer `compiled` owns; it takes no arguments and returns the double's
    // bits in RAX.
    let raw = unsafe { compiled.try_call(&[]).expect("test JIT call") };
    let calls = HELPER_CALLS.with(Cell::get) - before;
    (f64::from_bits(raw as u64), calls)
}

/// `float f() { return Math.fma(A, B, C); }`:
/// `ldc #1; ldc #2; ldc #3; invokestatic #4; freturn`.
fn run_float(a: f32, b: f32, c: f32) -> (f32, u32) {
    let code: Vec<u8> = vec![
        0x12, 0x01, // 0: ldc A
        0x12, 0x02, // 2: ldc B
        0x12, 0x03, // 4: ldc C
        0xb8, 0x00, 0x04, // 6: invokestatic Math.fma(FFF)F
        0xae, // 9: freturn
        0, 0,
    ];
    let compiled = compile(
        &code,
        10,
        0,
        0,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![(
            6,
            JitDirectCall {
                entry: entry("(FFF)F"),
                needs_context: false,
                num_params: 3,
                return_type: b'F',
                guard_class_id: 0,
            },
        )],
        Vec::new(), // mic_slots
        Vec::new(), // pic_slots
        vec![
            (0, i64::from(a.to_bits())),
            (2, i64::from(b.to_bits())),
            (4, i64::from(c.to_bits())),
        ],
        Vec::new(), // ldc2w_info
        HashMap::new(),
        HashMap::new(),
        &stub_helpers(),
        HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("single-pass compile of Math.fma(FFF)F");
    let before = HELPER_CALLS.with(Cell::get);
    // SAFETY: as in `run_double`; the float's bits come back in the low half
    // of RAX.
    let raw = unsafe { compiled.try_call(&[]).expect("test JIT call") };
    let calls = HELPER_CALLS.with(Cell::get) - before;
    (f32::from_bits(raw as u32), calls)
}

fn same64(x: f64, y: f64) -> bool {
    (x.is_nan() && y.is_nan()) || x.to_bits() == y.to_bits()
}

#[test]
fn r12w8_compiled_double_fma_is_inline_and_rounds_once() {
    let d = f64::from_bits;
    let cases: [(f64, f64, f64, u64); 7] = [
        // (1 + 2^-26)(1 + 2^-27) + 2^-200: the product is half-way between
        // two doubles and 2^-200 tips it up; a double-rounding fma answers
        // ...6000000.
        (
            d(0x3FF0_0000_0400_0000),
            d(0x3FF0_0000_0200_0000),
            d(0x3370_0000_0000_0000),
            0x3FF0_0000_0600_0001,
        ),
        // 0.1 * 10 - 1 is exactly 2^-54; unfused it is 0.0.
        (0.1, 10.0, -1.0, 0x3C90_0000_0000_0000),
        // Operand order: fma(2, 3, 1) = 7, fma(1, 3, 2) = 5.
        (2.0, 3.0, 1.0, 7.0f64.to_bits()),
        (1.0, 3.0, 2.0, 5.0f64.to_bits()),
        // The exact product is finite: -inf, where 1e308*10 + -inf is NaN.
        (1e308, 10.0, f64::NEG_INFINITY, f64::NEG_INFINITY.to_bits()),
        // Signed zeros: -1*0 + -0 = -0.
        (-1.0, 0.0, -0.0, 0x8000_0000_0000_0000),
        // inf * 0 is NaN.
        (f64::INFINITY, 0.0, 1.0, f64::NAN.to_bits()),
    ];
    let inline = expect_inline();
    for (a, b, c, want) in cases {
        let (got, calls) = run_double(a, b, c);
        if inline {
            assert_eq!(calls, 0, "fma({a:e}, {b:e}, {c:e}) reached the helper on an FMA3 host");
            assert!(
                same64(got, f64::from_bits(want)),
                "fma({a:e}, {b:e}, {c:e}) = {:#018x}, want {want:#018x}",
                got.to_bits()
            );
        } else {
            assert_eq!(calls, 1, "fma({a:e}, {b:e}, {c:e}) did not call the helper");
            assert!(same64(got, a.mul_add(b, c)), "the helper's answer is what the arm returns");
        }
    }
}

#[test]
fn r12w8_compiled_float_fma_is_inline_and_rounds_once() {
    let f = f32::from_bits;
    let cases: [(f32, f32, f32, u32); 4] = [
        // (1 + 2^-12)^2 + 2^-80: half-way in f32, tipped by 2^-80; the
        // `(float)((double)a*b + c)` shortcut answers 0x3F801000.
        (f(0x3F80_0800), f(0x3F80_0800), f(0x1780_0000), 0x3F80_1001),
        // 0.1f * 10 - 1 is exactly 2^-26; unfused it is 0.0.
        (0.1, 10.0, -1.0, 0x3280_0000),
        (2.0, 3.0, 1.0, 7.0f32.to_bits()),
        (1.0, 3.0, 2.0, 5.0f32.to_bits()),
    ];
    let inline = expect_inline();
    for (a, b, c, want) in cases {
        let (got, calls) = run_float(a, b, c);
        if inline {
            assert_eq!(calls, 0, "fmaf({a:e}, {b:e}, {c:e}) reached the helper on an FMA3 host");
            assert_eq!(
                got.to_bits(),
                want,
                "fmaf({a:e}, {b:e}, {c:e}) = {:#010x}, want {want:#010x}",
                got.to_bits()
            );
        } else {
            assert_eq!(calls, 1, "fmaf({a:e}, {b:e}, {c:e}) did not call the helper");
            assert_eq!(got.to_bits(), a.mul_add(b, c).to_bits());
        }
    }
}
