// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! r9 wave 7, lane `review7a`: the single-pass SPLICE walk lowers `float` /
//! `double` arithmetic, conversions, negation and compares.
//!
//! Before this the walk (`x64/inlining.rs` `try_emit_inline_body`) had no arm
//! for any of them, so the first `l2d` / `dmul` / `fcmpl` in a planned callee
//! rolled the whole splice back to a real call
//! (`inline-rollback OsrWideProbe$Sq.area(J)D at pc=95: callee_pc=5 op=0x8a`).
//!
//! Each test compiles a caller whose only call is an `invokestatic` backed by
//! an [`cratonvm_jit::InlineSite`] and NO invoke metadata, so the method can
//! only produce the right answer if the body was spliced: a refused splice
//! falls back to the dispatch helper, which here returns a sentinel no test
//! expects (or fails the compile outright). Values are Java's, worked by hand.

use cratonvm_jit::x64::compile;
use cratonvm_jit_api::JitRuntimeHelpers;
use std::collections::{HashMap, HashSet};

/// What the dispatch fallback answers if the splice was refused.
const NOT_SPLICED: i64 = 0x5EED_5EED;

fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("inline FP test reached an unwired runtime helper");
    }
    unsafe extern "C" fn dispatch_sentinel(_a: i64, _b: i64, _c: i64, _d: i64) -> i64 {
        NOT_SPLICED
    }
    unsafe extern "C" fn record_throw_bci(_bci: i64) {}
    unsafe extern "C" fn deopt_unserviceable_stub(
        _vm: i64,
        _info: i64,
        _args: i64,
        _n: i64,
    ) -> i64 {
        i64::MIN
    }
    let s = stub as *const () as usize;
    JitRuntimeHelpers {
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
        invoke_dispatch: dispatch_sentinel as *const () as usize,
        invoke_virtual_mic: s,
        lambda_int_to_double: s,
        write_barrier: s,
        satb_pre_write_barrier: s,
        uncommon_trap: s,
        math_fma_double: s,
        math_fma_float: s,
        throw_exception: s,
        jit_npe_with_action: s,
        dispatch_threw: s,
        jit_frem: s,
        jit_drem: s,
        ldc_string: s,
        set_throw_bci: record_throw_bci as *const () as usize,
        service_callee_deopt: deopt_unserviceable_stub as *const () as usize,
        ..Default::default()
    }
}

/// A static callee `descriptor` whose body is `callee` (no trailing padding).
fn site(
    callee: &[u8],
    max_locals: usize,
    num_args: usize,
    ret: u8,
    descriptor: &str,
) -> cratonvm_jit::InlineSite {
    let mut callee_code = callee.to_vec();
    callee_code.push(0);
    callee_code.push(0);
    cratonvm_jit::InlineSite {
        callee_code,
        callee_code_len: callee.len(),
        callee_max_locals: max_locals,
        callee_num_args: num_args,
        callee_is_static: true,
        return_type: ret,
        field_info: Vec::new(),
        compact_field_info: Vec::new(),
        static_field_info: Vec::new(),
        ldc_info: Vec::new(),
        ldc2w_info: Vec::new(),
        ldc_fp_pcs: Vec::new(),
        needs_heap: false,
        class_name: "R9w7Fp".to_string(),
        class_id: 0,
        method_name: "leaf".to_string(),
        descriptor: descriptor.to_string(),
        elided_invoke_pcs: Vec::new(),
        invoke_targets: Vec::new(),
        resolved_invoke_infos: Vec::new(),
        nested_sites: Vec::new(),
        ir_new_info: Vec::new(),
        ir_typecheck_info: Vec::new(),
        volatile_field_pcs: Vec::new(),
    }
}

/// `<load arg 0>; invokestatic #1; <return>` with `callee` spliced at pc 1.
fn compile_caller(
    load: u8,
    ret: u8,
    callee: cratonvm_jit::InlineSite,
) -> cratonvm_jit::CompiledMethod {
    let code: Vec<u8> = vec![load, 0xb8, 0x00, 0x01, ret, 0, 0];
    let mut sites = HashMap::new();
    sites.insert(1usize, callee);
    compile(
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
        Vec::new(), // invoke_info: none, so only a splice can answer
        Vec::new(), // direct_calls
        Vec::new(), // mic_slots
        Vec::new(), // pic_slots
        Vec::new(), // ldc_info
        Vec::new(), // ldc2w_info
        HashMap::new(),
        HashMap::new(),
        &helpers(),
        HashSet::new(),
        sites,
        None,
    )
    .expect("a caller whose FP leaf splices must compile")
}

/// `static long leaf(long a) { return (long) ((double) a * (double) a + 1.0); }`
/// -- `l2d`, `dmul`, `dconst_1`, `dadd`, `d2l` including its saturation.
#[test]
fn a_double_leaf_is_spliced_and_computes_javas_answer() {
    let callee = site(
        &[
            0x1e, // 0 lload_0
            0x8a, // 1 l2d
            0x1e, // 2 lload_0
            0x8a, // 3 l2d
            0x6b, // 4 dmul
            0x0f, // 5 dconst_1
            0x63, // 6 dadd
            0x8f, // 7 d2l
            0xad, // 8 lreturn
        ],
        2,
        1,
        b'J',
        "(J)J",
    );
    let m = compile_caller(0x1e, 0xad, callee);
    for (a, want) in [
        (3i64, 10i64),
        (-7, 50),
        (0, 1),
        // 2^62 + 1 rounds to 2^62 as a double.
        (1i64 << 31, 1i64 << 62),
        // 1.6e19 is past Long.MAX_VALUE: `d2l` saturates.
        (4_000_000_000, i64::MAX),
    ] {
        // SAFETY: JIT-compiled from valid bytecode; the body calls no helper
        // when the splice took.
        let got = unsafe { m.try_call(&[a]).expect("test JIT call") };
        assert_ne!(got, NOT_SPLICED, "the FP leaf was not spliced");
        assert_eq!(got, want, "leaf({a})");
    }
}

/// `static int leaf(int x) { return x / 2.0f > 1.0f ? 1 : 0; }` -- `i2f`,
/// `fdiv`, `fcmpl` feeding a callee branch.
#[test]
fn a_float_compare_feeding_a_callee_branch_is_spliced() {
    let callee = site(
        &[
            0x1a, // 0 iload_0
            0x86, // 1 i2f
            0x0d, // 2 fconst_2
            0x6e, // 3 fdiv
            0x0c, // 4 fconst_1
            0x95, // 5 fcmpl
            0x9e, 0x00, 0x05, // 6 ifle -> 11
            0x04, // 9 iconst_1
            0xac, // 10 ireturn
            0x03, // 11 iconst_0
            0xac, // 12 ireturn
        ],
        1,
        1,
        b'I',
        "(I)I",
    );
    let m = compile_caller(0x1a, 0xac, callee);
    for (x, want) in [(3i64, 1i64), (2, 0), (-9, 0), (1_000_000, 1)] {
        // SAFETY: as above.
        let got = unsafe { m.try_call(&[x]).expect("test JIT call") };
        assert_ne!(got, NOT_SPLICED, "the FP leaf was not spliced");
        assert_eq!(got, want, "leaf({x})");
    }
}

/// `static int leaf(int x) { return (int) -(double) -(float) x; }` spelled
/// through every width change: `i2f`, `fneg`, `f2d`, `dneg`, `d2f`, `f2i`.
#[test]
fn negation_and_width_conversions_round_trip_through_a_splice() {
    let callee = site(
        &[
            0x1a, // 0 iload_0
            0x86, // 1 i2f
            0x76, // 2 fneg
            0x8d, // 3 f2d
            0x77, // 4 dneg
            0x90, // 5 d2f
            0x8b, // 6 f2i
            0xac, // 7 ireturn
        ],
        1,
        1,
        b'I',
        "(I)I",
    );
    let m = compile_caller(0x1a, 0xac, callee);
    for x in [0i64, 1, -5, 123_456, -8_388_608] {
        // SAFETY: as above.
        let got = unsafe { m.try_call(&[x]).expect("test JIT call") };
        assert_ne!(got, NOT_SPLICED, "the FP leaf was not spliced");
        assert_eq!(got, x, "leaf({x})");
    }
}
