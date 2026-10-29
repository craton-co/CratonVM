// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The residuals of JIT round 21's lane vector (book
//! `docs/feature-designs/jit-ir-optimizations.md`), executed with each switch off and
//! on against a host-side Java reference: VEC21-2 (widening loads,
//! `CRATONVM_JIT_IR_VECTOR_WIDEN`, and the `long` lane multiply,
//! `CRATONVM_JIT_IR_VECTOR_LONG_MUL`, and narrowing stores,
//! `CRATONVM_JIT_IR_VECTOR_NARROW`), VEC21-6 (shifts by a loop-invariant
//! count, `CRATONVM_JIT_IR_VECTOR_VARSHIFT`), and two Windows-pool shapes for
//! VEC21-7 (`CRATONVM_JIT_IR_VECTOR_SAVE_XMM`, the r23 L8 close's prologue
//! save), with a check that a caller's XMM8-XMM15 survive.
//!
//! Every test asserts the answer in BOTH arms (the off arm is the scalar
//! loop's control) and, on an AVX2 host, that the on arm emitted a pre-loop
//! (its body is larger) -- so a planner that silently refuses the shape is a
//! failure here, not a pass.

use cratonvm_jit::{CachedBytecodeMethod, CompileRequest, CompiledMethod, JitRuntimeHelpers};
use cratonvm_types::{ClassId, ARRAY_LENGTH_OFFSET, HEADER_SIZE};
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("r21_vector_residuals invoked an unwired runtime helper");
    }
    unsafe extern "C" fn self_guard(_vm_ptr: i64) -> i64 {
        0
    }
    unsafe extern "C" fn native_stack_floor() -> i64 {
        0
    }
    unsafe extern "C" fn record_throw_bci(_bci: i64) {}
    let s = stub as *const () as usize;
    // Wide-open region bounds (as `ir_vs_singlepass`), leaked rather than a
    // static: these tests touch arrays only.
    let bounds: &'static [AtomicUsize; 6] = Box::leak(Box::new([
        AtomicUsize::new(0x1000),
        AtomicUsize::new(usize::MAX),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ]));
    JitRuntimeHelpers {
        safepoint_flag_addr: 0,
        safepoint_slow_path: 0,
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
        tlab_end_offset_in_thread: 8,
        throw_exception: s,
        jit_npe_with_action: s,
        dispatch_threw: s,
        jit_frem: s,
        jit_drem: s,
        self_call_stack_guard: self_guard as *const () as usize,
        region_bounds_addr: bounds.as_ptr() as usize,
        read_bounds_addr: bounds.as_ptr() as usize,
        native_stack_floor_fn: native_stack_floor as *const () as usize,
        ldc_string: s,
        set_throw_bci: record_throw_bci as *const () as usize,
        ..Default::default()
    }
}

fn cached(
    name: &str,
    descriptor: &str,
    mut code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
) -> CachedBytecodeMethod {
    // The VM pads every method's bytecode with two zero bytes.
    code.push(0x00);
    code.push(0x00);
    CachedBytecodeMethod {
        declaring_class_id: ClassId::new(1),
        class_name: Arc::from("R21VecRes"),
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
        hidden_frame: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
        array_inc_declined: std::sync::OnceLock::new(),
        ctor_verdict_memo: std::sync::OnceLock::new(),
        pool_generation: u64::MAX,
    }
}

/// Compile on the optimizing tier, `invokestatic #2` resolving to `target`.
fn compile(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    target: Option<(&str, &str, &str)>,
    fp: bool,
) -> CompiledMethod {
    cratonvm_jit::ir_evidence::force_accept_always_for_this_process();
    // #2 is the method under test's target; #3 / #4 are the raw-bits pair the
    // FP test uses to move a double through the GPR call ABI.
    let resolver = move |cp: u16| -> Option<(String, String, String)> {
        let row = |c: &str, m: &str, d: &str| Some((c.to_string(), m.to_string(), d.to_string()));
        match cp {
            2 => {
                let (c, m, d) = target?;
                row(c, m, d)
            }
            3 => row("java/lang/Double", "doubleToRawLongBits", "(D)J"),
            4 => row("java/lang/Double", "longBitsToDouble", "(J)D"),
            _ => None,
        }
    };
    // The gates the VM's own C2 request sets (`gates(static special virtual)`).
    let go = || {
        cratonvm_jit::try_compile_request(&CompileRequest {
            cp_invoke_resolver: Some(&resolver),
            optimize: true,
            ir_emit_calls: true,
            ir_emit_special_calls: true,
            ir_emit_virtual_calls: true,
            ir_emit_long: true,
            ir_emit_fp: fp,
            ..CompileRequest::new(cm, helpers, cratonvm_jit::CompileRealm::process())
        })
        .unwrap_or_else(|| panic!("{} compiles", cm.method_name))
    };
    let compiled = go();
    if !compiled.used_ir_backend {
        // (A nested `with_thread_overrides` REPLACES the caller's set, so no
        // DBG re-run here: rerun the test with `CRATONVM_DBG_JITC=1` in the
        // environment for the `[ir]` refusal line.)
        panic!(
            "{} fell back to single-pass (rerun with CRATONVM_DBG_JITC=1)",
            cm.method_name
        );
    }
    compiled
}

fn call(m: &CompiledMethod, args: &[i64]) -> i64 {
    let vm = [0u8; 64];
    // SAFETY: each method below takes arrays this test built (live for the
    // call) and ints, and every index it touches is below the array length or
    // is answered by the scalar loop's own check.
    let r = unsafe {
        if m.needs_context() {
            m.try_call_with_context(vm.as_ptr() as i64, args)
        } else {
            m.try_call(args)
        }
    };
    r.expect("call")
}

fn prim_array(width: usize, elems: &[i64]) -> Vec<u8> {
    let mut buf = vec![0u8; HEADER_SIZE + elems.len() * width];
    buf[ARRAY_LENGTH_OFFSET..ARRAY_LENGTH_OFFSET + 4]
        .copy_from_slice(&(elems.len() as u32).to_le_bytes());
    for (i, &v) in elems.iter().enumerate() {
        let off = HEADER_SIZE + i * width;
        buf[off..off + width].copy_from_slice(&v.to_le_bytes()[..width]);
    }
    buf
}

fn assert_grew(name: &str, sizes: &[usize]) {
    if cratonvm_jit::x64::has_avx2() {
        assert!(
            sizes[1] > sizes[0],
            "{name}: the switched-on arm emitted no pre-loop ({sizes:?})"
        );
    }
}

fn read_elems(buf: &[u8], width: usize, n: usize) -> Vec<i64> {
    (0..n)
        .map(|i| {
            let off = HEADER_SIZE + i * width;
            let mut w = [0u8; 8];
            w[..width].copy_from_slice(&buf[off..off + width]);
            // Sign-extend from `width` bytes.
            let sh = 64 - 8 * width as u32;
            (i64::from_le_bytes(w) << sh) >> sh
        })
        .collect()
}

/// Lengths around the step and trip boundaries of every lane width.
const NS: [usize; 16] = [0, 1, 3, 4, 5, 7, 8, 9, 15, 16, 17, 31, 32, 33, 66, 71];

/// VEC21-2: `static void f(int[] d, char[] c, byte[] b, int n) { for (int j
/// = 0; j < n; j++) d[j] = c[j] - b[j]; }` -- `caload` zero-extends,
/// `baload` sign-extends, both widened at the load into `int` lanes.
#[test]
fn vector_widen_char_minus_byte_into_int() {
    let code = vec![
        0x03, // 0: iconst_0
        0x36, 0x04, // 1: istore 4         j = 0
        0x15, 0x04, // 3: iload 4          (header)
        0x1d, // 5: iload_3             n
        0xa2, 0x00, 0x16, // 6: if_icmpge -> 28
        0x2a, // 9: aload_0
        0x15, 0x04, // 10: iload 4
        0x2b, // 12: aload_1
        0x15, 0x04, // 13: iload 4
        0x34, // 15: caload
        0x2c, // 16: aload_2
        0x15, 0x04, // 17: iload 4
        0x33, // 19: baload
        0x64, // 20: isub
        0x4f, // 21: iastore
        0x84, 0x04, 0x01, // 22: iinc 4, 1
        0xa7, 0xff, 0xea, // 25: goto -> 3
        0xb1, // 28: return
    ];
    let len = 71usize;
    let c0: Vec<i64> = (0..len)
        .map(|i| match i {
            0 => 0xFFFF,
            5 => 0x8000,
            _ => (i as i64 * 40_503 + 7) & 0xFFFF,
        })
        .collect();
    let b0: Vec<i64> = (0..len)
        .map(|i| ((i as i64 * 91 + 200) & 0xFF) as i8 as i64)
        .collect();
    let h = helpers();
    let cm = cached("cwiden", "([I[C[BI)V", code, 5, 4);
    let mut sizes = Vec::new();
    for arm in [Some("0"), None] {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_VECTOR_WIDEN", arm)],
            || {
                let m = compile(&cm, &h, None, false);
                sizes.push(m.code_len());
                for n in NS {
                    let d = prim_array(4, &vec![-7; len]);
                    let c = prim_array(2, &c0);
                    let b = prim_array(1, &b0);
                    call(
                        &m,
                        &[
                            d.as_ptr() as i64,
                            c.as_ptr() as i64,
                            b.as_ptr() as i64,
                            n as i64,
                        ],
                    );
                    let want: Vec<i64> = (0..len)
                        .map(|j| if j < n { (c0[j] & 0xFFFF) - b0[j] } else { -7 })
                        .collect();
                    assert_eq!(read_elems(&d, 4, len), want, "arm={arm:?} n={n}");
                }
            },
        );
    }
    assert_grew("cwiden", &sizes);
}

/// VEC21-2 + the `long` lane multiply: `static void f(long[] d, X[] a, long
/// k, int n) { for (int j = 0; j < n; j++) d[j] = a[j] * k; }` for `X` each
/// of `int`, `char`, `short`, `byte` (`i2l` folded into the widening load) --
/// `k` near the 64-bit edges, so every partial product of the three
/// `VPMULUDQ` matters.
#[test]
fn vector_widen_to_long_times_k() {
    // (load opcode, descriptor, width, sign-extending)
    let kinds = [
        (0x2eu8, "([J[IJI)V", 4usize, true),
        (0x34, "([J[CJI)V", 2, false),
        (0x35, "([J[SJI)V", 2, true),
        (0x33, "([J[BJI)V", 1, true),
    ];
    for (op, desc, width, signed) in kinds {
        let code = vec![
            0x03, // 0: iconst_0
            0x36, 0x05, // 1: istore 5         j = 0
            0x15, 0x05, // 3: iload 5          (header)
            0x15, 0x04, // 5: iload 4          n
            0xa2, 0x00, 0x14, // 7: if_icmpge -> 27
            0x2a, // 10: aload_0
            0x15, 0x05, // 11: iload 5
            0x2b, // 13: aload_1
            0x15, 0x05, // 14: iload 5
            op,   // 16: xaload
            0x85, // 17: i2l
            0x20, // 18: lload_2            k
            0x69, // 19: lmul
            0x50, // 20: lastore
            0x84, 0x05, 0x01, // 21: iinc 5, 1
            0xa7, 0xff, 0xeb, // 24: goto -> 3
            0xb1, // 27: return
        ];
        let len = 41usize;
        let bits = 8 * width as u32;
        let a0: Vec<i64> = (0..len)
            .map(|i| {
                let raw = (i as i64)
                    .wrapping_mul(0x9E37_79B9)
                    .wrapping_add(0x7FFF_FFF0);
                let raw = if i == 2 { -1 } else { raw };
                let lo = raw & ((1i64 << bits) - 1);
                if signed {
                    (lo << (64 - bits)) >> (64 - bits)
                } else {
                    lo
                }
            })
            .collect();
        let h = helpers();
        let cm = cached("lmulk", desc, code, 6, 4);
        let mut sizes = Vec::new();
        for arm in [Some("0"), None] {
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_JIT_IR_VECTOR_WIDEN", arm),
                    ("CRATONVM_JIT_IR_VECTOR_LONG_MUL", arm),
                ],
                || {
                    let m = compile(&cm, &h, None, false);
                    sizes.push(m.code_len());
                    for k in [3i64, -1, i64::MIN + 3, 0x1_0000_0001, 0x7FFF_FFFF_FFFF_FFFF] {
                        for n in NS.iter().copied().filter(|&n| n <= len) {
                            let d = prim_array(8, &vec![99; len]);
                            let a = prim_array(width, &a0);
                            call(&m, &[d.as_ptr() as i64, a.as_ptr() as i64, k, n as i64]);
                            let want: Vec<i64> = (0..len)
                                .map(|j| if j < n { a0[j].wrapping_mul(k) } else { 99 })
                                .collect();
                            assert_eq!(
                                read_elems(&d, 8, len),
                                want,
                                "{desc} arm={arm:?} k={k} n={n}"
                            );
                        }
                    }
                },
            );
        }
        assert_grew(desc, &sizes);
    }
}

/// VEC21-2, FP: `static void f(double[] d, float[] f, double k, int n) {
/// for (int j = 0; j < n; j++) d[j] = f[j] * k; }` -- `VCVTPS2PD` is `f2d`
/// lane by lane: NaNs (payload included), `-0.0`, subnormals, infinities.
#[test]
fn vector_widen_float_to_double_times_k() {
    let code = vec![
        0x03, // 0: iconst_0
        0x36, 0x05, // 1: istore 5         j = 0
        0x15, 0x05, // 3: iload 5          (header)
        0x15, 0x04, // 5: iload 4          n
        0xa2, 0x00, 0x14, // 7: if_icmpge -> 27
        0x2a, // 10: aload_0
        0x15, 0x05, // 11: iload 5
        0x2b, // 13: aload_1
        0x15, 0x05, // 14: iload 5
        0x30, // 16: faload
        0x8d, // 17: f2d
        0x28, // 18: dload_2            k
        0x6b, // 19: dmul
        0x52, // 20: dastore
        0x84, 0x05, 0x01, // 21: iinc 5, 1
        0xa7, 0xff, 0xeb, // 24: goto -> 3
        0xb1, // 27: return
    ];
    let len = 37usize;
    let f0: Vec<f32> = (0..len)
        .map(|i| match i {
            1 => f32::from_bits(0x7FC0_1234),
            2 => -0.0,
            3 => f32::from_bits(1),
            4 => f32::INFINITY,
            9 => f32::from_bits(0xFFA0_0001),
            13 => f32::MAX,
            _ => (i as f32) * -1.375 + 3.0,
        })
        .collect();
    let bits: Vec<i64> = f0.iter().map(|x| i64::from(x.to_bits())).collect();
    let h = helpers();
    let cm = cached("fwiden", "([D[FDI)V", code, 6, 4);
    let mut sizes = Vec::new();
    for arm in [Some("0"), None] {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_VECTOR_WIDEN", arm)],
            || {
                let m = compile(&cm, &h, None, true);
                sizes.push(m.code_len());
                for k in [0.5f64, -3.0, 1e300] {
                    for n in NS.iter().copied().filter(|&n| n <= len) {
                        let d = prim_array(8, &vec![5; len]);
                        let f = prim_array(4, &bits);
                        call(
                            &m,
                            &[
                                d.as_ptr() as i64,
                                f.as_ptr() as i64,
                                k.to_bits() as i64,
                                n as i64,
                            ],
                        );
                        let want: Vec<i64> = (0..len)
                            .map(|j| {
                                if j < n {
                                    (f64::from(f0[j]) * k).to_bits() as i64
                                } else {
                                    5
                                }
                            })
                            .collect();
                        assert_eq!(read_elems(&d, 8, len), want, "arm={arm:?} k={k} n={n}");
                    }
                }
            },
        );
    }
    assert_grew("fwiden", &sizes);
}

/// VEC21-2 through a reduction: `static int f(X[] a, int n) { int s = 0; for
/// (int j = 0; j < n; j++) s += a[j]; return s; }` for `byte[]` and
/// `char[]` -- the lanes are the accumulator's `int`.
#[test]
fn vector_widen_subword_sum_into_int() {
    for (op, desc, width, signed) in [(0x33u8, "([BI)I", 1usize, true), (0x34, "([CI)I", 2, false)]
    {
        let code = vec![
            0x03, // 0: iconst_0
            0x3d, // 1: istore_2            s = 0
            0x03, // 2: iconst_0
            0x3e, // 3: istore_3            j = 0
            0x1d, // 4: iload_3             (header)
            0x1b, // 5: iload_1             n
            0xa2, 0x00, 0x0f, // 6: if_icmpge -> 21
            0x1c, // 9: iload_2
            0x2a, // 10: aload_0
            0x1d, // 11: iload_3
            op,   // 12: xaload
            0x60, // 13: iadd
            0x3d, // 14: istore_2
            0x84, 0x03, 0x01, // 15: iinc 3, 1
            0xa7, 0xff, 0xf2, // 18: goto -> 4
            0x1c, // 21: iload_2
            0xac, // 22: ireturn
        ];
        let len = 300usize;
        let bits = 8 * width as u32;
        let a0: Vec<i64> = (0..len)
            .map(|i| {
                let lo = (i as i64 * 40_503 + 250) & ((1i64 << bits) - 1);
                if signed {
                    (lo << (64 - bits)) >> (64 - bits)
                } else {
                    lo
                }
            })
            .collect();
        let arr = prim_array(width, &a0);
        let h = helpers();
        let cm = cached("swsum", desc, code, 4, 2);
        let mut sizes = Vec::new();
        for arm in [Some("0"), None] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_JIT_IR_VECTOR_WIDEN", arm)],
                || {
                    let m = compile(&cm, &h, None, false);
                    sizes.push(m.code_len());
                    for n in NS.iter().copied().chain([127, 128, 129, 299, 300]) {
                        let got = call(&m, &[arr.as_ptr() as i64, n as i64]);
                        let want = a0[..n].iter().fold(0i32, |s, &x| s.wrapping_add(x as i32));
                        assert_eq!(got as i32, want, "{desc} arm={arm:?} n={n}");
                    }
                },
            );
        }
        assert_grew(desc, &sizes);
    }
}

/// VEC21-6: `static void f(int[] c, int[] a, int k, int n) { for (int j = 0;
/// j < n; j++) c[j] = ((a[j] << k) ^ (a[j] >>> k)) ^ (a[j] >> k); }` -- one
/// count register for the three shifts; `k` over every masking corner (the
/// x86 count form zeroes a lane for a count >= 32, Java masks `k & 31`).
#[test]
fn vector_varshift_int_masks_the_count_like_java() {
    let code = vec![
        0x03, // 0: iconst_0
        0x36, 0x04, // 1: istore 4         j = 0
        0x15, 0x04, // 3: iload 4          (header)
        0x1d, // 5: iload_3             n
        0xa2, 0x00, 0x21, // 6: if_icmpge -> 39
        0x2a, // 9: aload_0
        0x15, 0x04, // 10: iload 4
        0x2b, // 12: aload_1
        0x15, 0x04, // 13: iload 4
        0x2e, // 15: iaload
        0x1c, // 16: iload_2            k
        0x78, // 17: ishl
        0x2b, // 18: aload_1
        0x15, 0x04, // 19: iload 4
        0x2e, // 21: iaload
        0x1c, // 22: iload_2
        0x7c, // 23: iushr
        0x82, // 24: ixor
        0x2b, // 25: aload_1
        0x15, 0x04, // 26: iload 4
        0x2e, // 28: iaload
        0x1c, // 29: iload_2
        0x7a, // 30: ishr
        0x82, // 31: ixor
        0x4f, // 32: iastore
        0x84, 0x04, 0x01, // 33: iinc 4, 1
        0xa7, 0xff, 0xdf, // 36: goto -> 3
        0xb1, // 39: return
    ];
    let len = 45usize;
    let a0: Vec<i64> = (0..len)
        .map(|i| i64::from((i as i32).wrapping_mul(0x1357_9BDF) ^ i32::MIN))
        .collect();
    let h = helpers();
    let cm = cached("vshift", "([I[III)V", code, 5, 4);
    let mut sizes = Vec::new();
    for arm in [Some("0"), None] {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_VECTOR_VARSHIFT", arm)],
            || {
                let m = compile(&cm, &h, None, false);
                sizes.push(m.code_len());
                for k in [0i32, 1, 7, 31, 32, 33, 40, -1, i32::MIN, i32::MAX] {
                    for n in NS.iter().copied().filter(|&n| n <= len) {
                        let c = prim_array(4, &vec![0; len]);
                        let a = prim_array(4, &a0);
                        call(
                            &m,
                            &[c.as_ptr() as i64, a.as_ptr() as i64, i64::from(k), n as i64],
                        );
                        let want: Vec<i64> = (0..len)
                            .map(|j| {
                                if j >= n {
                                    return 0;
                                }
                                let x = a0[j] as i32;
                                let s = (k & 31) as u32;
                                i64::from((x << s) ^ ((x as u32) >> s) as i32 ^ (x >> s))
                            })
                            .collect();
                        assert_eq!(read_elems(&c, 4, len), want, "arm={arm:?} k={k} n={n}");
                    }
                }
            },
        );
    }
    assert_grew("vshift", &sizes);
}

/// VEC21-6, `long` lanes: `static void f(long[] c, long[] a, int k, int n) {
/// for (int j = 0; j < n; j++) c[j] = (a[j] << k) + (a[j] >>> k); }` --
/// `k & 63` (`>>` has no AVX2 form on `long` and stays refused).
#[test]
fn vector_varshift_long_masks_the_count_like_java() {
    let code = vec![
        0x03, // 0: iconst_0
        0x36, 0x04, // 1: istore 4         j = 0
        0x15, 0x04, // 3: iload 4          (header)
        0x1d, // 5: iload_3             n
        0xa2, 0x00, 0x1a, // 6: if_icmpge -> 32
        0x2a, // 9: aload_0
        0x15, 0x04, // 10: iload 4
        0x2b, // 12: aload_1
        0x15, 0x04, // 13: iload 4
        0x2f, // 15: laload
        0x1c, // 16: iload_2            k
        0x79, // 17: lshl
        0x2b, // 18: aload_1
        0x15, 0x04, // 19: iload 4
        0x2f, // 21: laload
        0x1c, // 22: iload_2
        0x7d, // 23: lushr
        0x61, // 24: ladd
        0x50, // 25: lastore
        0x84, 0x04, 0x01, // 26: iinc 4, 1
        0xa7, 0xff, 0xe6, // 29: goto -> 3
        0xb1, // 32: return
    ];
    let len = 23usize;
    let a0: Vec<i64> = (0..len)
        .map(|i| (i as i64).wrapping_mul(0x1E37_79B9_7F4A_7C15) ^ i64::MIN)
        .collect();
    let h = helpers();
    let cm = cached("vlshift", "([J[JII)V", code, 5, 4);
    let mut sizes = Vec::new();
    for arm in [Some("0"), None] {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_VECTOR_VARSHIFT", arm)],
            || {
                let m = compile(&cm, &h, None, false);
                sizes.push(m.code_len());
                for k in [0i32, 1, 31, 32, 63, 64, 65, -1, i32::MIN] {
                    for n in NS.iter().copied().filter(|&n| n <= len) {
                        let c = prim_array(8, &vec![0; len]);
                        let a = prim_array(8, &a0);
                        call(
                            &m,
                            &[c.as_ptr() as i64, a.as_ptr() as i64, i64::from(k), n as i64],
                        );
                        let want: Vec<i64> = (0..len)
                            .map(|j| {
                                if j >= n {
                                    return 0;
                                }
                                let s = (k & 63) as u32;
                                (a0[j] << s).wrapping_add(((a0[j] as u64) >> s) as i64)
                            })
                            .collect();
                        assert_eq!(read_elems(&c, 8, len), want, "arm={arm:?} k={k} n={n}");
                    }
                }
            },
        );
    }
    assert_grew("vlshift", &sizes);
}

/// VEC21-7: `static void f(int[] c, int[] a, int[] b, int k, int m, int n) {
/// for (int j = 0; j < n; j++) c[j] = a[j] * k + b[j] * m; }` -- two
/// invariants and two temporaries, four vector registers: Windows used to
/// refuse it (its pool was XMM0/XMM1); with VEC21-7 a body with a vector plan
/// saves XMM6-XMM11 in its prologue, so the pre-loop has XMM8-XMM11 there too.
/// The switch moves nothing on System V. A Windows caller's XMM8-XMM15 must
/// come back unchanged: every call goes through a trampoline that seeds them
/// and checks them after the call.
#[test]
fn vector_xmm_save_admits_a_four_register_tree_on_windows() {
    let code = vec![
        0x03, // 0: iconst_0
        0x36, 0x06, // 1: istore 6         j = 0
        0x15, 0x06, // 3: iload 6          (header)
        0x15, 0x05, // 5: iload 5          n
        0xa2, 0x00, 0x1b, // 7: if_icmpge -> 34
        0x2a, // 10: aload_0
        0x15, 0x06, // 11: iload 6
        0x2b, // 13: aload_1
        0x15, 0x06, // 14: iload 6
        0x2e, // 16: iaload
        0x1d, // 17: iload_3            k
        0x68, // 18: imul
        0x2c, // 19: aload_2
        0x15, 0x06, // 20: iload 6
        0x2e, // 22: iaload
        0x15, 0x04, // 23: iload 4          m
        0x68, // 25: imul
        0x60, // 26: iadd
        0x4f, // 27: iastore
        0x84, 0x06, 0x01, // 28: iinc 6, 1
        0xa7, 0xff, 0xe4, // 31: goto -> 3
        0xb1, // 34: return
    ];
    let len = 70usize;
    let a0: Vec<i64> = (0..len).map(|i| i32::MAX as i64 - i as i64 * 977).collect();
    let b0: Vec<i64> = (0..len).map(|i| 3 + i as i64 * 65_537).collect();
    let h = helpers();
    let cm = cached("axpy", "([I[I[IIII)V", code, 7, 6);
    let mut sizes = Vec::new();
    for arm in [Some("0"), None] {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_VECTOR_SAVE_XMM", arm)],
            || {
                let m = compile(&cm, &h, None, false);
                sizes.push(m.code_len());
                for (k, m_inv) in [(3i32, 7i32), (-1, i32::MIN), (i32::MIN, -5)] {
                    for n in NS.iter().copied().chain([69, 70]) {
                        let c = prim_array(4, &vec![0; len]);
                        let a = prim_array(4, &a0);
                        let b = prim_array(4, &b0);
                        let args = [
                            c.as_ptr() as i64,
                            a.as_ptr() as i64,
                            b.as_ptr() as i64,
                            i64::from(k),
                            i64::from(m_inv),
                            n as i64,
                        ];
                        let kept = call_keeping_xmm8_15(&m, &args);
                        assert!(
                            kept,
                            "arm={arm:?} k={k} n={n}: a caller's XMM8-XMM15 changed"
                        );
                        let want: Vec<i64> = (0..len)
                            .map(|j| {
                                if j < n {
                                    i64::from(
                                        (a0[j] as i32)
                                            .wrapping_mul(k)
                                            .wrapping_add((b0[j] as i32).wrapping_mul(m_inv)),
                                    )
                                } else {
                                    0
                                }
                            })
                            .collect();
                        assert_eq!(read_elems(&c, 4, len), want, "arm={arm:?} k={k} n={n}");
                    }
                }
            },
        );
    }
    if cfg!(windows) {
        assert_grew("axpy", &sizes);
    } else {
        assert_eq!(
            sizes[0], sizes[1],
            "axpy: the switch has no effect on System V"
        );
    }
}

/// Call `m` (no VM context) through a trampoline that loads distinct values
/// into XMM8-XMM15 (saving the test's own around it, Win64 makes them
/// non-volatile), calls, and reports whether all eight came back unchanged.
/// On System V the registers are volatile and nothing is checked.
fn call_keeping_xmm8_15(m: &CompiledMethod, args: &[i64]) -> bool {
    if !cfg!(windows) || !cratonvm_jit::x64::has_avx2() || m.needs_context() {
        call(m, args);
        return true;
    }
    // The block the trampoline reads: [0] = the entry, [1..7] = up to six
    // arguments, [8] = the verdict it writes.
    let mut block = [0i64; 10];
    block[0] = m.entry_ptr() as i64;
    block[1..1 + args.len()].copy_from_slice(args);
    let mut buf = cratonvm_jit::ExecutableBuffer::new(4096).expect("executable buffer");
    // Win64: RCX = &block. PUSH RBX (RSP is then 16-aligned); SUB RSP, 176:
    // home space, the 5th and 6th arguments at +32 / +40, the saves at +48.
    buf.emit(&[0x53]);
    buf.emit(&[0x48, 0x81, 0xEC]);
    buf.emit(&(128i32 + 48).to_le_bytes());
    buf.emit(&[0x48, 0x89, 0xCB]); // MOV RBX, RCX
    for i in 0..8u8 {
        // MOVDQU [RSP + 48 + 16i], xmm(8+i)  (F3 REX.R 0F 7F /r)
        buf.emit(&[0xF3, 0x44, 0x0F, 0x7F, 0x84 | (i << 3), 0x24]);
        buf.emit(&(48i32 + 16 * i32::from(i)).to_le_bytes());
        // MOV RAX, imm64 (a per-register pattern); MOVQ xmm(8+i), RAX
        buf.emit(&[0x48, 0xB8]);
        buf.emit(
            &(0x0123_4567_89AB_CDEFu64.rotate_left(u32::from(i) * 8) ^ u64::from(i)).to_le_bytes(),
        );
        buf.emit(&[0x66, 0x4C, 0x0F, 0x6E, 0xC0 | (i << 3)]);
    }
    // Args: RCX, RDX, R8, R9 from the block, the 5th and 6th on the stack.
    buf.emit(&[0x48, 0x8B, 0x43, 0x28]); // MOV RAX, [RBX + 40]
    buf.emit(&[0x48, 0x89, 0x44, 0x24, 0x20]); // MOV [RSP + 32], RAX
    buf.emit(&[0x48, 0x8B, 0x43, 0x30]); // MOV RAX, [RBX + 48]
    buf.emit(&[0x48, 0x89, 0x44, 0x24, 0x28]); // MOV [RSP + 40], RAX
    buf.emit(&[0x48, 0x8B, 0x4B, 0x08]); // MOV RCX, [RBX + 8]
    buf.emit(&[0x48, 0x8B, 0x53, 0x10]); // MOV RDX, [RBX + 16]
    buf.emit(&[0x4C, 0x8B, 0x43, 0x18]); // MOV R8, [RBX + 24]
    buf.emit(&[0x4C, 0x8B, 0x4B, 0x20]); // MOV R9, [RBX + 32]
    buf.emit(&[0xFF, 0x13]); // CALL [RBX]
                             // RCX = 0 (all equal); per register: MOVQ RAX, xmm; compare with the seed.
    buf.emit(&[0x31, 0xC9]); // XOR ECX, ECX
    for i in 0..8u8 {
        buf.emit(&[0x66, 0x4C, 0x0F, 0x7E, 0xC0 | (i << 3)]); // MOVQ RAX, xmm(8+i)
        buf.emit(&[0x48, 0xBA]); // MOV RDX, imm64
        buf.emit(
            &(0x0123_4567_89AB_CDEFu64.rotate_left(u32::from(i) * 8) ^ u64::from(i)).to_le_bytes(),
        );
        buf.emit(&[0x48, 0x31, 0xD0]); // XOR RAX, RDX
        buf.emit(&[0x48, 0x09, 0xC1]); // OR RCX, RAX
                                       // MOVDQU xmm(8+i), [RSP + 48 + 16i]: restore the test's own.
        buf.emit(&[0xF3, 0x44, 0x0F, 0x6F, 0x84 | (i << 3), 0x24]);
        buf.emit(&(48i32 + 16 * i32::from(i)).to_le_bytes());
    }
    buf.emit(&[0x48, 0x89, 0x4B, 0x40]); // MOV [RBX + 64], RCX
    buf.emit(&[0x48, 0x81, 0xC4]);
    buf.emit(&(128i32 + 48).to_le_bytes()); // ADD RSP, ..
    buf.emit(&[0x5B, 0xC3]); // POP RBX; RET
    assert!(!buf.overflowed());
    buf.finalize();
    // SAFETY: the trampoline is a complete Win64 `extern "C" fn(*mut i64)`
    // that saves and restores the non-volatile registers it touches (RBX,
    // XMM8-XMM15) and calls the compiled method with the arrays this test
    // built, live for the call.
    let f = unsafe { std::mem::transmute::<*const u8, extern "C" fn(*mut i64)>(buf.as_ptr()) };
    f(block.as_mut_ptr());
    block[8] == 0
}

/// VEC21-7 for VEC21-5's register-hungry arms (r23 L8-2): `static long
/// lmax(long[] a, long m, int n) { for (int j = 0; j < n; j++) m =
/// Math.max(m, a[j]); return m; }` -- a `long` min/max step holds the
/// accumulator and two temporaries, more than Windows' XMM0/XMM1.
#[test]
fn vector_xmm_save_admits_a_long_max_reduction() {
    let code = vec![
        0x03, // 0: iconst_0
        0x36, 0x04, // 1: istore 4         j = 0
        0x15, 0x04, // 3: iload 4          (header)
        0x1d, // 5: iload_3             n
        0xa2, 0x00, 0x12, // 6: if_icmpge -> 24
        0x1f, // 9: lload_1             m
        0x2a, // 10: aload_0
        0x15, 0x04, // 11: iload 4
        0x2f, // 13: laload
        0xb8, 0x00, 0x02, // 14: invokestatic #2  Math.max(JJ)J
        0x40, // 17: lstore_1
        0x84, 0x04, 0x01, // 18: iinc 4, 1
        0xa7, 0xff, 0xee, // 21: goto -> 3
        0x1f, // 24: lload_1
        0xad, // 25: lreturn
    ];
    let len = 41usize;
    let a: Vec<i64> = (0..len)
        .map(|i| match i {
            7 => i64::MAX,
            20 => i64::MIN,
            _ => (i as i64).wrapping_mul(0x5DEE_CE66_D1CE_4E5B) >> 3,
        })
        .collect();
    let arr = prim_array(8, &a);
    let h = helpers();
    let cm = cached("lmax", "([JJI)J", code, 5, 3);
    let mut sizes = Vec::new();
    for arm in [Some("0"), None] {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_VECTOR_SAVE_XMM", arm)],
            || {
                let m = compile(&cm, &h, Some(("java/lang/Math", "max", "(JJ)J")), false);
                sizes.push(m.code_len());
                for seed in [i64::MIN, -1, 0, i64::MAX] {
                    for n in NS.iter().copied().filter(|&n| n <= len).chain([40, 41]) {
                        let got = call(&m, &[arr.as_ptr() as i64, seed, n as i64]);
                        let want = a[..n].iter().fold(seed, |m, &x| m.max(x));
                        assert_eq!(got, want, "arm={arm:?} seed={seed} n={n}");
                    }
                }
            },
        );
    }
    if cfg!(windows) {
        assert_grew("lmax", &sizes);
    } else {
        assert_eq!(
            sizes[0], sizes[1],
            "lmax: the switch has no effect on System V"
        );
    }
}

/// VEC21-2, narrowing: `static void f(T[] d, int[] a, int k, int n) { for
/// (int j = 0; j < n; j++) d[j] = (T) (a[j] * k); }` for `T` each of `byte`,
/// `char`, `short` -- `int` lanes truncated at the store, as
/// `bastore`/`castore`/`sastore` truncate. The `byte` arm also stores into a
/// `boolean[]`, which keeps `value & 1` (JVMS 6.5): the scalar loop's.
#[test]
fn vector_narrow_int_into_subword_stores() {
    // (narrowing op, store op, descriptor, width)
    for (conv, store, desc, width) in [
        (0x91u8, 0x54u8, "([B[III)V", 1usize),
        (0x92, 0x55, "([C[III)V", 2),
        (0x93, 0x56, "([S[III)V", 2),
    ] {
        let code = vec![
            0x03, // 0: iconst_0
            0x36, 0x04, // 1: istore 4         j = 0
            0x15, 0x04, // 3: iload 4          (header)
            0x1d, // 5: iload_3             n
            0xa2, 0x00, 0x14, // 6: if_icmpge -> 26
            0x2a, // 9: aload_0
            0x15, 0x04, // 10: iload 4
            0x2b, // 12: aload_1
            0x15, 0x04,  // 13: iload 4
            0x2e,  // 15: iaload
            0x1c,  // 16: iload_2            k
            0x68,  // 17: imul
            conv,  // 18: i2b / i2c / i2s
            store, // 19: bastore / castore / sastore
            0x84, 0x04, 0x01, // 20: iinc 4, 1
            0xa7, 0xff, 0xec, // 23: goto -> 3
            0xb1, // 26: return
        ];
        let len = 71usize;
        let a0: Vec<i64> = (0..len)
            .map(|i| i64::from((i as i32).wrapping_mul(0x0765_4321) ^ (i as i32) << 9))
            .collect();
        let h = helpers();
        let cm = cached("narrow", desc, code, 5, 4);
        let mut sizes = Vec::new();
        for arm in [Some("0"), None] {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_JIT_IR_VECTOR_NARROW", arm)],
                || {
                    let m = compile(&cm, &h, None, false);
                    sizes.push(m.code_len());
                    for k in [1i32, 3, -7, 0x0101_0101] {
                        for n in NS {
                            let d = prim_array(width, &vec![0x5A; len]);
                            let a = prim_array(4, &a0);
                            call(
                                &m,
                                &[d.as_ptr() as i64, a.as_ptr() as i64, i64::from(k), n as i64],
                            );
                            let trunc = |x: i32| -> i64 {
                                match conv {
                                    0x91 => i64::from(x as i8),
                                    0x92 => i64::from(x as u16 as i16),
                                    _ => i64::from(x as i16),
                                }
                            };
                            let want: Vec<i64> = (0..len)
                                .map(|j| {
                                    if j < n {
                                        trunc((a0[j] as i32).wrapping_mul(k))
                                    } else {
                                        trunc(0x5A)
                                    }
                                })
                                .collect();
                            assert_eq!(
                                read_elems(&d, width, len),
                                want,
                                "{desc} arm={arm:?} k={k} n={n}"
                            );
                            if conv != 0x91 {
                                continue;
                            }
                            let mut z = prim_array(1, &vec![0; len]);
                            z[cratonvm_types::KIND_TAGS_BYTE_OFFSET] =
                                cratonvm_types::primitive_array_kind_tags_byte("[Z")
                                    .expect("[Z tag");
                            call(
                                &m,
                                &[z.as_ptr() as i64, a.as_ptr() as i64, i64::from(k), n as i64],
                            );
                            let want: Vec<i64> = (0..len)
                                .map(|j| {
                                    if j < n {
                                        i64::from((a0[j] as i32).wrapping_mul(k) & 1)
                                    } else {
                                        0
                                    }
                                })
                                .collect();
                            assert_eq!(
                                read_elems(&z, 1, len),
                                want,
                                "boolean arm={arm:?} k={k} n={n}"
                            );
                        }
                    }
                },
            );
        }
        assert_grew(desc, &sizes);
    }
}

/// VEC21-2, narrowing with the destination also read (widened):
/// `static void f(byte[] b, int[] a, int n) { for (int j = 0; j < n; j++)
/// b[j] = (byte) (b[j] + a[j]); }` -- every lane is read before its store.
#[test]
fn vector_narrow_reads_its_own_destination_first() {
    let code = vec![
        0x03, // 0: iconst_0
        0x3e, // 1: istore_3            j = 0
        0x1d, // 2: iload_3             (header)
        0x1c, // 3: iload_2             n
        0xa2, 0x00, 0x14, // 4: if_icmpge -> 24
        0x2a, // 7: aload_0
        0x1d, // 8: iload_3
        0x2a, // 9: aload_0
        0x1d, // 10: iload_3
        0x33, // 11: baload
        0x2b, // 12: aload_1
        0x1d, // 13: iload_3
        0x2e, // 14: iaload
        0x60, // 15: iadd
        0x91, // 16: i2b
        0x54, // 17: bastore
        0x84, 0x03, 0x01, // 18: iinc 3, 1
        0xa7, 0xff, 0xed, // 21: goto -> 2
        0xb1, // 24: return
    ];
    let len = 71usize;
    let a0: Vec<i64> = (0..len)
        .map(|i| i64::from((i as i32) * 0x0123_4567))
        .collect();
    let b0: Vec<i64> = (0..len)
        .map(|i| ((i as i64 * 91 + 200) & 0xFF) as i8 as i64)
        .collect();
    let h = helpers();
    let cm = cached("narrowrw", "([B[II)V", code, 4, 3);
    let mut sizes = Vec::new();
    for arm in [Some("0"), None] {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_VECTOR_NARROW", arm)],
            || {
                let m = compile(&cm, &h, None, false);
                sizes.push(m.code_len());
                for n in NS {
                    let b = prim_array(1, &b0);
                    let a = prim_array(4, &a0);
                    call(&m, &[b.as_ptr() as i64, a.as_ptr() as i64, n as i64]);
                    let want: Vec<i64> = (0..len)
                        .map(|j| {
                            if j < n {
                                i64::from((b0[j] as i32).wrapping_add(a0[j] as i32) as i8)
                            } else {
                                b0[j]
                            }
                        })
                        .collect();
                    assert_eq!(read_elems(&b, 1, len), want, "arm={arm:?} n={n}");
                }
            },
        );
    }
    assert_grew("narrowrw", &sizes);
}
