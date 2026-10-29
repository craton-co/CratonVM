// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 23 wave 1, lane L8: the IR vector pre-loop's VEC21-5 (exact
//! `Math.min/max` reductions, `CRATONVM_JIT_IR_VECTOR_MINMAX`) and VEC21-8
//! (early-exit scans, `CRATONVM_JIT_IR_VECTOR_SCAN`), executed with the switch
//! off and on against a host-side Java reference. Book:
//! `docs/feature-designs/jit-ir-optimizations.md`.
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
        panic!("r23_w1_l8_vector_minmax_scan invoked an unwired runtime helper");
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
        class_name: Arc::from("R23L8"),
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
            5 => row("java/lang/Float", "intBitsToFloat", "(I)F"),
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

/// VEC21-8: `static int find(byte[] a, int c, int n) { for (int j = 0; j < n;
/// j++) if (a[j] == c) return j; return -1; }` -- every hit position around
/// the 32-lane step and 128-element trip boundaries, a second later hit, no
/// hit, and `c = 200` (a value no `byte` holds: the pre-loop's range guard
/// leaves for the scalar loop, which finds nothing).
#[test]
fn vector_scan_byte_eq_finds_the_first_hit() {
    let code = vec![
        0x03, // 0: iconst_0
        0x3e, // 1: istore_3            j = 0
        0x1d, // 2: iload_3             (header)
        0x1c, // 3: iload_2             n
        0xa2, 0x00, 0x12, // 4: if_icmpge -> 22
        0x2a, // 7: aload_0
        0x1d, // 8: iload_3
        0x33, // 9: baload
        0x1b, // 10: iload_1            c
        0xa0, 0x00, 0x05, // 11: if_icmpne -> 16
        0x1d, // 14: iload_3
        0xac, // 15: ireturn
        0x84, 0x03, 0x01, // 16: iinc 3, 1
        0xa7, 0xff, 0xef, // 19: goto -> 2
        0x02, // 22: iconst_m1
        0xac, // 23: ireturn
    ];
    let len = 300usize;
    let h = helpers();
    let cm = cached("find", "([BII)I", code, 4, 3);
    let mut sizes = Vec::new();
    for arm in [Some("0"), None] {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_VECTOR_SCAN", arm)],
            || {
                let m = compile(&cm, &h, None, false);
                sizes.push(m.code_len());
                for c in [-56i32, 200, 127] {
                    for hit in [
                        None,
                        Some(0),
                        Some(31),
                        Some(32),
                        Some(127),
                        Some(128),
                        Some(299),
                    ] {
                        let mut a: Vec<i64> = (0..len).map(|i| ((i * 7) % 120) as i64).collect();
                        if let Some(p) = hit {
                            a[p] = i64::from(c as i8);
                            if p + 5 < len {
                                a[p + 5] = i64::from(c as i8);
                            }
                        }
                        let arr = prim_array(1, &a);
                        for n in [0usize, 1, 31, 32, 33, 127, 128, 129, 160, 299, 300] {
                            let got = call(&m, &[arr.as_ptr() as i64, i64::from(c), n as i64]);
                            let want = (0..n)
                                .find(|&j| i32::from(a[j] as i8) == c)
                                .map_or(-1, |j| j as i32);
                            assert_eq!(got as i32, want, "arm={arm:?} c={c} hit={hit:?} n={n}");
                        }
                    }
                }
            },
        );
    }
    assert_grew("find", &sizes);
}

/// VEC21-8 with a miss mask: `static int skip(int[] a, int c, int n) { int j
/// = 0; while (j < n && a[j] < c) j++; return j; }` -- the loop leaves on
/// `a[j] >= c`, which the pre-loop tests as NOT `c > a[j]`.
#[test]
fn vector_scan_int_while_less_stops_at_the_first_not_less() {
    let code = vec![
        0x03, // 0: iconst_0
        0x3e, // 1: istore_3            j = 0
        0x1d, // 2: iload_3             (header)
        0x1c, // 3: iload_2             n
        0xa2, 0x00, 0x10, // 4: if_icmpge -> 20
        0x2a, // 7: aload_0
        0x1d, // 8: iload_3
        0x2e, // 9: iaload
        0x1b, // 10: iload_1            c
        0xa2, 0x00, 0x09, // 11: if_icmpge -> 20
        0x84, 0x03, 0x01, // 14: iinc 3, 1
        0xa7, 0xff, 0xf1, // 17: goto -> 2
        0x1d, // 20: iload_3
        0xac, // 21: ireturn
    ];
    let len = 100usize;
    let h = helpers();
    let cm = cached("skip", "([III)I", code, 4, 3);
    let mut sizes = Vec::new();
    for arm in [Some("0"), None] {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_VECTOR_SCAN", arm)],
            || {
                let m = compile(&cm, &h, None, false);
                sizes.push(m.code_len());
                for c in [0i32, i32::MIN, i32::MAX, -7] {
                    for stop in [None, Some(0), Some(7), Some(8), Some(33), Some(99)] {
                        let mut a: Vec<i64> =
                            (0..len).map(|i| i64::from(i32::MIN) + i as i64).collect();
                        if let Some(p) = stop {
                            a[p] = i64::from(c);
                        }
                        let arr = prim_array(4, &a);
                        for n in [0usize, 1, 8, 9, 32, 33, 40, 99, 100] {
                            let got = call(&m, &[arr.as_ptr() as i64, i64::from(c), n as i64]);
                            let want = (0..n).find(|&j| a[j] as i32 >= c).unwrap_or(n);
                            assert_eq!(
                                got as i32, want as i32,
                                "arm={arm:?} c={c} stop={stop:?} n={n}"
                            );
                        }
                    }
                }
            },
        );
    }
    assert_grew("skip", &sizes);
}

/// VEC21-5: `static int imax(int[] a, int m, int n) { for (int j = 0; j < n;
/// j++) m = Math.max(m, a[j]); return m; }` -- MIN/MAX values, odd lengths.
#[test]
fn vector_minmax_int_max_reduction_matches_the_scalar_fold() {
    let code = vec![
        0x03, // 0: iconst_0
        0x3e, // 1: istore_3            j = 0
        0x1d, // 2: iload_3             (header)
        0x1c, // 3: iload_2             n
        0xa2, 0x00, 0x11, // 4: if_icmpge -> 21
        0x1b, // 7: iload_1             m
        0x2a, // 8: aload_0
        0x1d, // 9: iload_3
        0x2e, // 10: iaload
        0xb8, 0x00, 0x02, // 11: invokestatic #2  Math.max(II)I
        0x3c, // 14: istore_1
        0x84, 0x03, 0x01, // 15: iinc 3, 1
        0xa7, 0xff, 0xf0, // 18: goto -> 2
        0x1b, // 21: iload_1
        0xac, // 22: ireturn
    ];
    let len = 77usize;
    let a: Vec<i64> = (0..len)
        .map(|i| match i {
            13 => i64::from(i32::MAX),
            40 => i64::from(i32::MIN),
            _ => (i as i64 * 7_919 - 300_000) ^ 0x55,
        })
        .collect();
    let arr = prim_array(4, &a);
    let h = helpers();
    let cm = cached("imax", "([III)I", code, 4, 3);
    let mut sizes = Vec::new();
    for arm in [Some("0"), None] {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_VECTOR_MINMAX", arm)],
            || {
                let m = compile(&cm, &h, Some(("java/lang/Math", "max", "(II)I")), false);
                sizes.push(m.code_len());
                for seed in [i32::MIN, -1, 0, i32::MAX] {
                    for n in 0..=len {
                        let got = call(&m, &[arr.as_ptr() as i64, i64::from(seed), n as i64]);
                        let want = a[..n].iter().fold(seed, |m, &x| m.max(x as i32));
                        assert_eq!(got as i32, want, "arm={arm:?} seed={seed} n={n}");
                    }
                }
            },
        );
    }
    assert_grew("imax", &sizes);
}

/// JDK 25 `Math.max(double, double)`, bit for bit.
fn jmax(a: f64, b: f64) -> f64 {
    if a.is_nan() {
        return a;
    }
    if a == 0.0 && b == 0.0 && a.to_bits() == (-0.0f64).to_bits() {
        return b;
    }
    if a >= b {
        a
    } else {
        b
    }
}

/// VEC21-5, FP: `static long dmax(double[] a, long seed, int n) { double m =
/// Double.longBitsToDouble(seed); for (int j = 0; j < n; j++) m =
/// Math.max(m, a[j]); return Double.doubleToRawLongBits(m); }` under
/// `CRATONVM_JIT_IR_FP_MATH_OPS=1` (the FP min/max sites' own opt-in). The
/// raw-bits pair keeps every argument and the answer in GPRs -- the shape the
/// VM's own `R23W1L8VecRows.dmax` admits (the w1 form, which read and wrote
/// the seed through a `double[]`, fell back to single-pass in this harness).
/// Java-exact bits: `-0.0` vs `+0.0` in every lane position, NaNs with
/// distinct payloads (the FIRST one in program order wins), a NaN seed.
#[test]
fn vector_minmax_double_max_reduction_is_java_exact() {
    let code = vec![
        0x1f, // 0: lload_1
        0xb8, 0x00, 0x04, // 1: invokestatic #4  Double.longBitsToDouble(J)D
        0x39, 0x04, // 4: dstore 4         m
        0x03, // 6: iconst_0
        0x36, 0x06, // 7: istore 6         j = 0
        0x15, 0x06, // 9: iload 6          (header)
        0x1d, // 11: iload_3            n
        0xa2, 0x00, 0x14, // 12: if_icmpge -> 32
        0x18, 0x04, // 15: dload 4
        0x2a, // 17: aload_0
        0x15, 0x06, // 18: iload 6
        0x31, // 20: daload
        0xb8, 0x00, 0x02, // 21: invokestatic #2  Math.max(DD)D
        0x39, 0x04, // 24: dstore 4
        0x84, 0x06, 0x01, // 26: iinc 6, 1
        0xa7, 0xff, 0xec, // 29: goto -> 9
        0x18, 0x04, // 32: dload 4
        0xb8, 0x00, 0x03, // 34: invokestatic #3  Double.doubleToRawLongBits(D)J
        0xad, // 37: lreturn
    ];
    let len = 37usize;
    let nan_a = f64::from_bits(0x7FF0_0000_0000_0123);
    let nan_b = f64::from_bits(0xFFF8_0000_0000_0042);
    let mut sets: Vec<Vec<f64>> = Vec::new();
    // All -0.0 but one +0.0 at each of several lane positions.
    for p in [None, Some(0), Some(3), Some(17), Some(36)] {
        let mut v = vec![-0.0f64; len];
        if let Some(p) = p {
            v[p] = 0.0;
        }
        sets.push(v);
    }
    sets.push(
        (0..len)
            .map(|i| ((i as f64) * 1.25 - 20.0) * if i % 3 == 0 { -1.0 } else { 1.0 })
            .collect(),
    );
    for (p, q) in [(0usize, 20usize), (5, 6), (36, 1), (30, 31)] {
        let mut v: Vec<f64> = (0..len).map(|i| i as f64 - 18.0).collect();
        v[p] = nan_a;
        v[q] = nan_b;
        sets.push(v);
    }
    let h = helpers();
    let cm = cached("dmax", "([DJI)J", code, 7, 3);
    let mut sizes = Vec::new();
    for arm in [Some("0"), None] {
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_IR_VECTOR_MINMAX", arm),
                ("CRATONVM_JIT_IR_FP_MATH_OPS", Some("1")),
            ],
            || {
                let m = compile(&cm, &h, Some(("java/lang/Math", "max", "(DD)D")), true);
                sizes.push(m.code_len());
                for v in &sets {
                    let bits: Vec<i64> = v.iter().map(|x| x.to_bits() as i64).collect();
                    let arr = prim_array(8, &bits);
                    for seed in [f64::NEG_INFINITY, -0.0, 0.0, f64::NAN] {
                        for n in [0usize, 1, 3, 4, 5, 16, 17, 20, 21, 31, 36, 37] {
                            let got =
                                call(&m, &[arr.as_ptr() as i64, seed.to_bits() as i64, n as i64])
                                    as u64;
                            let want = v[..n].iter().fold(seed, |m, &x| jmax(m, x)).to_bits();
                            assert_eq!(
                                got,
                                want,
                                "arm={arm:?} seed={seed} n={n} v={v:?}: got {:e}",
                                f64::from_bits(got)
                            );
                        }
                    }
                }
            },
        );
    }
    // An FP min/max step needs four vector registers (accumulator, NaN-seen,
    // two temporaries). On Windows they fit since VEC21-7 (the frame of a
    // body with a vector plan saves XMM8-XMM11).
    assert_grew("dmax", &sizes);
}

/// BCE21-5's vector side: `static int sum(int[] a, int n) { int s = 0; for
/// (int j = 0; j != n; j++) s += a[j]; return s; }` -- a `!=` loop test is
/// admitted like `<` (the pre-loop steps only while `j + 8 <= n`).
#[test]
fn vector_ne_exit_int_sum_matches_the_scalar_sum() {
    let code = vec![
        0x03, // 0: iconst_0
        0x3d, // 1: istore_2            s = 0
        0x03, // 2: iconst_0
        0x3e, // 3: istore_3            j = 0
        0x1d, // 4: iload_3             (header)
        0x1b, // 5: iload_1             n
        0x9f, 0x00, 0x0f, // 6: if_icmpeq -> 21
        0x1c, // 9: iload_2
        0x2a, // 10: aload_0
        0x1d, // 11: iload_3
        0x2e, // 12: iaload
        0x60, // 13: iadd
        0x3d, // 14: istore_2
        0x84, 0x03, 0x01, // 15: iinc 3, 1
        0xa7, 0xff, 0xf2, // 18: goto -> 4
        0x1c, // 21: iload_2
        0xac, // 22: ireturn
    ];
    let len = 61usize;
    let a: Vec<i64> = (0..len)
        .map(|i| i64::from(i32::MAX) - i as i64 * 1_000_003)
        .collect();
    let arr = prim_array(4, &a);
    let h = helpers();
    let cm = cached("nesum", "([II)I", code, 4, 2);
    let mut sizes = Vec::new();
    for arm in [Some("0"), None] {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_VECTOR_NE_EXIT", arm)],
            || {
                let m = compile(&cm, &h, None, false);
                sizes.push(m.code_len());
                for n in 0..=len {
                    let got = call(&m, &[arr.as_ptr() as i64, n as i64]);
                    let want = a[..n].iter().fold(0i32, |s, &x| s.wrapping_add(x as i32));
                    assert_eq!(got as i32, want, "arm={arm:?} n={n}");
                }
            },
        );
    }
    assert_grew("nesum", &sizes);
}

/// BCE21-5 (r23 wave 2): `static int sumNe(int[] a) { int s = 0; for (int j =
/// 0; j != a.length; j++) s += a[j]; return s; }` -- with
/// `CRATONVM_JIT_IR_BCE_NE_EXIT` the `a[j]` bounds check goes (the body is
/// smaller); the answer is the same in both arms. The vector pre-loop is off
/// in both, so the size difference is the check alone.
#[test]
fn bce_ne_exit_drops_the_bounds_check_of_a_ne_length_loop() {
    let code = vec![
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1            s = 0
        0x03, // 2: iconst_0
        0x3d, // 3: istore_2            j = 0
        0x1c, // 4: iload_2             (header)
        0x2a, // 5: aload_0
        0xbe, // 6: arraylength
        0x9f, 0x00, 0x0f, // 7: if_icmpeq -> 22
        0x1b, // 10: iload_1
        0x2a, // 11: aload_0
        0x1c, // 12: iload_2
        0x2e, // 13: iaload
        0x60, // 14: iadd
        0x3c, // 15: istore_1
        0x84, 0x02, 0x01, // 16: iinc 2, 1
        0xa7, 0xff, 0xf1, // 19: goto -> 4
        0x1b, // 22: iload_1
        0xac, // 23: ireturn
    ];
    let h = helpers();
    let cm = cached("sumNe", "([I)I", code, 3, 1);
    let mut sizes = Vec::new();
    for arm in [Some("0"), None] {
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_IR_BCE_NE_EXIT", arm),
                ("CRATONVM_JIT_IR_VECTOR_LOOP", Some("0")),
            ],
            || {
                let m = compile(&cm, &h, None, false);
                sizes.push(m.code_len());
                for len in [0usize, 1, 2, 7, 33] {
                    let a: Vec<i64> = (0..len).map(|i| i as i64 * 7 - 3).collect();
                    let arr = prim_array(4, &a);
                    let got = call(&m, &[arr.as_ptr() as i64]);
                    let want = a.iter().fold(0i32, |s, &x| s.wrapping_add(x as i32));
                    assert_eq!(got as i32, want, "arm={arm:?} len={len}");
                }
            },
        );
    }
    assert!(
        sizes[1] < sizes[0],
        "sumNe: the switched-on arm kept its bounds check ({sizes:?})"
    );
}

// ── r23 L8 close: L8-3 (element-wise min/max) and L8-4 (wider scans) ──────
//
// Bytecode below is javac 25's for the Java line each test quotes, with the
// `invokestatic` indices remapped to this file's resolver (#2 the method
// under test's target, #4 `Double.longBitsToDouble`, #5
// `Float.intBitsToFloat`).

/// The `width`-byte elements of a `prim_array` buffer, as raw bits.
fn elems(buf: &[u8], width: usize) -> Vec<u64> {
    let n = u32::from_le_bytes(
        buf[ARRAY_LENGTH_OFFSET..ARRAY_LENGTH_OFFSET + 4]
            .try_into()
            .unwrap(),
    ) as usize;
    (0..n)
        .map(|i| {
            let off = HEADER_SIZE + i * width;
            let mut w = [0u8; 8];
            w[..width].copy_from_slice(&buf[off..off + width]);
            u64::from_le_bytes(w)
        })
        .collect()
}

/// JDK 25 `Math.min(float, float)`, bit for bit.
fn jminf(a: f32, b: f32) -> f32 {
    if a.is_nan() {
        return a;
    }
    if a == 0.0 && b == 0.0 && b.to_bits() == (-0.0f32).to_bits() {
        return b;
    }
    if a <= b {
        a
    } else {
        b
    }
}

/// Run a `(c, a, b, n)` store kernel over every `n` up to the length, with
/// `c` reset each time, and compare `c` with `want(a[j], b[j])` below `n`
/// and its reset value above.
#[allow(clippy::too_many_arguments)]
fn check_store_kernel(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    target: (&str, &str, &str),
    width: usize,
    fp: bool,
    a: &[i64],
    b: &[i64],
    want: &dyn Fn(u64, u64) -> u64,
) {
    let len = a.len();
    let h = helpers();
    let cm = cached(name, descriptor, code, 5, 4);
    let mut sizes = Vec::new();
    for arm in [Some("0"), None] {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_VECTOR_ELEM_MINMAX", arm)],
            || {
                let m = compile(&cm, &h, Some(target), fp);
                sizes.push(m.code_len());
                let aa = prim_array(width, a);
                let bb = prim_array(width, b);
                let av = elems(&aa, width);
                let bv = elems(&bb, width);
                for n in 0..=len {
                    let mut c = prim_array(width, &vec![0x5A5A_5A5A_5A5A_5A5A; len]);
                    let reset = elems(&c, width)[0];
                    call(
                        &m,
                        &[
                            c.as_mut_ptr() as i64,
                            aa.as_ptr() as i64,
                            bb.as_ptr() as i64,
                            n as i64,
                        ],
                    );
                    let got = elems(&c, width);
                    for j in 0..len {
                        let w = if j < n { want(av[j], bv[j]) } else { reset };
                        assert_eq!(got[j], w, "{name} arm={arm:?} n={n} j={j}");
                    }
                }
            },
        );
    }
    assert_grew(name, &sizes);
}

/// L8-3: `for (j..n) c[j] = Math.max(a[j], b[j])` over `int` lanes, and
/// `Math.min` over `long` lanes, MIN/MAX values included.
#[test]
fn elem_minmax_int_and_long_store_loops_match_math() {
    let code = vec![
        0x03, 0x36, 0x04, 0x15, 0x04, 0x1d, 0xa2, 0x00, 0x18, 0x2a, 0x15, 0x04, 0x2b, 0x15, 0x04,
        0x2e, 0x2c, 0x15, 0x04, 0x2e, 0xb8, 0x00, 0x02, 0x4f, 0x84, 0x04, 0x01, 0xa7, 0xff, 0xe8,
        0xb1,
    ];
    let a: Vec<i64> = (0..45)
        .map(|i| match i {
            3 => i64::from(i32::MIN),
            17 => i64::from(i32::MAX),
            _ => (i * 7919 - 150_000) ^ 0x33,
        })
        .collect();
    let b: Vec<i64> = (0..45)
        .map(|i| match i {
            4 => i64::from(i32::MIN),
            17 => i64::from(i32::MIN),
            _ => 100_000 - i * 4_001,
        })
        .collect();
    check_store_kernel(
        "emaxI",
        "([I[I[II)V",
        code,
        ("java/lang/Math", "max", "(II)I"),
        4,
        false,
        &a,
        &b,
        &|x, y| (x as u32 as i32).max(y as u32 as i32) as u32 as u64,
    );
    let code = vec![
        0x03, 0x36, 0x04, 0x15, 0x04, 0x1d, 0xa2, 0x00, 0x18, 0x2a, 0x15, 0x04, 0x2b, 0x15, 0x04,
        0x2f, 0x2c, 0x15, 0x04, 0x2f, 0xb8, 0x00, 0x02, 0x50, 0x84, 0x04, 0x01, 0xa7, 0xff, 0xe8,
        0xb1,
    ];
    let a: Vec<i64> = (0..23)
        .map(|i| match i {
            0 => i64::MIN,
            9 => i64::MAX,
            _ => (i as i64).wrapping_mul(0x1_0000_0001) - (1 << 40),
        })
        .collect();
    let b: Vec<i64> = a
        .iter()
        .enumerate()
        .map(|(i, &x)| if i % 3 == 0 { x } else { -x + i as i64 })
        .collect();
    check_store_kernel(
        "eminL",
        "([J[J[JI)V",
        code,
        ("java/lang/Math", "min", "(JJ)J"),
        8,
        false,
        &a,
        &b,
        &|x, y| (x as i64).min(y as i64) as u64,
    );
}

/// L8-3, FP: `c[j] = Math.max(a[j], b[j])` (double) and `Math.min` (float)
/// with `-0.0` / `+0.0` in both orders, infinities, and NaNs of distinct
/// payloads on either side and on both -- Java's bits in every lane.
#[test]
fn elem_minmax_fp_store_loops_are_java_exact() {
    let nan_a = f64::from_bits(0x7FF0_0000_0000_0123);
    let nan_b = f64::from_bits(0xFFF8_0000_0000_0042);
    let xs = [
        0.0,
        -0.0,
        1.0,
        nan_a,
        -0.0,
        5.0,
        f64::NEG_INFINITY,
        2.0,
        nan_b,
        -1.0,
        0.0,
    ];
    let ys = [
        -0.0,
        0.0,
        nan_b,
        1.0,
        -0.0,
        -5.0,
        3.0,
        2.0,
        nan_a,
        f64::INFINITY,
        0.0,
    ];
    let a: Vec<i64> = (0..37).map(|i| xs[i % 11].to_bits() as i64).collect();
    let b: Vec<i64> = (0..37).map(|i| ys[(i * 3) % 11].to_bits() as i64).collect();
    let code = vec![
        0x03, 0x36, 0x04, 0x15, 0x04, 0x1d, 0xa2, 0x00, 0x18, 0x2a, 0x15, 0x04, 0x2b, 0x15, 0x04,
        0x31, 0x2c, 0x15, 0x04, 0x31, 0xb8, 0x00, 0x02, 0x52, 0x84, 0x04, 0x01, 0xa7, 0xff, 0xe8,
        0xb1,
    ];
    check_store_kernel(
        "emaxD",
        "([D[D[DI)V",
        code,
        ("java/lang/Math", "max", "(DD)D"),
        8,
        true,
        &a,
        &b,
        &|x, y| jmax(f64::from_bits(x), f64::from_bits(y)).to_bits(),
    );
    let fa: Vec<i64> = (0..37)
        .map(|i| i64::from((xs[i % 11] as f32).to_bits()))
        .collect();
    let fb: Vec<i64> = (0..37)
        .map(|i| i64::from((ys[(i * 5) % 11] as f32).to_bits()))
        .collect();
    let code = vec![
        0x03, 0x36, 0x04, 0x15, 0x04, 0x1d, 0xa2, 0x00, 0x18, 0x2a, 0x15, 0x04, 0x2b, 0x15, 0x04,
        0x30, 0x2c, 0x15, 0x04, 0x30, 0xb8, 0x00, 0x02, 0x51, 0x84, 0x04, 0x01, 0xa7, 0xff, 0xe8,
        0xb1,
    ];
    check_store_kernel(
        "eminF",
        "([F[F[FI)V",
        code,
        ("java/lang/Math", "min", "(FF)F"),
        4,
        true,
        &fa,
        &fb,
        &|x, y| u64::from(jminf(f32::from_bits(x as u32), f32::from_bits(y as u32)).to_bits()),
    );
}

/// Compile `code` with the wide-scan switch off and on; for each argument
/// list, the answer must equal `want` in both arms.
#[allow(clippy::too_many_arguments)]
fn check_scan_kernel(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
    fp: bool,
    cases: &[(Vec<i64>, i64)],
) {
    let h = helpers();
    let cm = cached(name, descriptor, code, max_locals, num_params);
    let mut sizes = Vec::new();
    for arm in [Some("0"), None] {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_VECTOR_SCAN_WIDE", arm)],
            || {
                let m = compile(&cm, &h, None, fp);
                sizes.push(m.code_len());
                for (args, want) in cases {
                    let got = call(&m, args);
                    assert_eq!(got as i32, *want as i32, "{name} arm={arm:?} args={args:?}");
                }
            },
        );
    }
    assert_grew(name, &sizes);
}

/// The lengths every scan test runs (step and trip boundaries for 32-, 16-,
/// 8- and 4-lane steps).
const SCAN_NS: [usize; 13] = [0, 1, 3, 4, 7, 8, 9, 15, 16, 17, 31, 33, 70];

/// L8-4: `if (a[j] == c1 || a[j] == c2) return j;` over byte lanes (two
/// compares ORed), and `if (a[j] >= lo && a[j] <= hi) return j;` over int
/// lanes (a complemented compare inside an AND).
#[test]
fn wide_scan_two_way_tests_find_the_first_hit() {
    let code = vec![
        0x03, 0x36, 0x04, 0x15, 0x04, 0x1d, 0xa2, 0x00, 0x1c, 0x2a, 0x15, 0x04, 0x33, 0x1b, 0x9f,
        0x00, 0x0b, 0x2a, 0x15, 0x04, 0x33, 0x1c, 0xa0, 0x00, 0x06, 0x15, 0x04, 0xac, 0x84, 0x04,
        0x01, 0xa7, 0xff, 0xe4, 0x02, 0xac,
    ];
    let len = 70usize;
    let mut bufs: Vec<(Vec<i64>, Vec<u8>)> = Vec::new();
    for (p, q) in [
        (None, None),
        (Some(0), None),
        (Some(40), Some(9)),
        (None, Some(69)),
    ] {
        let mut a: Vec<i64> = (0..len).map(|i| (i % 50) as i64 + 1).collect();
        if let Some(p) = p {
            a[p] = -100;
        }
        if let Some(q) = q {
            a[q] = 120;
        }
        let buf = prim_array(1, &a);
        bufs.push((a, buf));
    }
    let mut cases = Vec::new();
    for (a, buf) in &bufs {
        for (c1, c2) in [(-100i64, 120i64), (120, 300), (7, -100), (999, 1000)] {
            for n in SCAN_NS {
                let want = (0..n)
                    .find(|&j| a[j] == c1 || a[j] == c2)
                    .map_or(-1, |j| j as i64);
                cases.push((vec![buf.as_ptr() as i64, c1, c2, n as i64], want));
            }
        }
    }
    check_scan_kernel("findTwo", "([BIII)I", code, 5, 4, false, &cases);

    let code = vec![
        0x03, 0x36, 0x04, 0x15, 0x04, 0x1d, 0xa2, 0x00, 0x1c, 0x2a, 0x15, 0x04, 0x2e, 0x1b, 0xa1,
        0x00, 0x0e, 0x2a, 0x15, 0x04, 0x2e, 0x1c, 0xa3, 0x00, 0x06, 0x15, 0x04, 0xac, 0x84, 0x04,
        0x01, 0xa7, 0xff, 0xe4, 0x02, 0xac,
    ];
    let a: Vec<i64> = (0..len)
        .map(|i| match i {
            11 => i64::from(i32::MIN),
            12 => i64::from(i32::MAX),
            _ => (i as i64 * 37) % 1000 - 500,
        })
        .collect();
    let buf = prim_array(4, &a);
    let mut cases = Vec::new();
    for (lo, hi) in [
        (400i64, 410i64),
        (i64::from(i32::MIN), i64::from(i32::MIN)),
        (i64::from(i32::MAX), i64::from(i32::MAX)),
        (10, 5),
        (-500, 499),
    ] {
        for n in SCAN_NS {
            let want = (0..n)
                .find(|&j| a[j] >= lo && a[j] <= hi)
                .map_or(-1, |j| j as i64);
            cases.push((vec![buf.as_ptr() as i64, lo, hi, n as i64], want));
        }
    }
    check_scan_kernel("findIn", "([IIII)I", code, 5, 4, false, &cases);
}

/// L8-4, element trees: `(a[j] & 0xFF) == c` over int lanes, and
/// `a[j] - '0' < 10` over a `char[]` (widened to int lanes: in 16-bit lanes
/// `c - 48` would wrap for `c >= 0x8030`).
#[test]
fn wide_scan_element_tree_tests_are_value_exact() {
    let code = vec![
        0x03, 0x3e, 0x1d, 0x1c, 0xa2, 0x00, 0x16, 0x2a, 0x1d, 0x2e, 0x11, 0x00, 0xff, 0x7e, 0x1b,
        0xa0, 0x00, 0x05, 0x1d, 0xac, 0x84, 0x03, 0x01, 0xa7, 0xff, 0xeb, 0x02, 0xac,
    ];
    let len = 70usize;
    let a: Vec<i64> = (0..len)
        .map(|i| (i as i64 * 0x0101_0007) & 0x7FFF_FF00 | ((i as i64 * 3) & 0x7F))
        .collect();
    let buf = prim_array(4, &a);
    let mut cases = Vec::new();
    for c in [0i64, 9, 0x7E, 0x1FF, a[33] & 0xFF, a[69] & 0xFF] {
        for n in SCAN_NS {
            let want = (0..n)
                .find(|&j| (a[j] & 0xFF) == c)
                .map_or(-1, |j| j as i64);
            cases.push((vec![buf.as_ptr() as i64, c, n as i64], want));
        }
    }
    check_scan_kernel("findMask", "([III)I", code, 4, 3, false, &cases);

    let code = vec![
        0x03, 0x3d, 0x1c, 0x1b, 0xa2, 0x00, 0x16, 0x2a, 0x1c, 0x34, 0x10, 0x30, 0x64, 0x10, 0x0a,
        0xa2, 0x00, 0x05, 0x1c, 0xac, 0x84, 0x02, 0x01, 0xa7, 0xff, 0xeb, 0x02, 0xac,
    ];
    let mut cases = Vec::new();
    let mut bufs = Vec::new();
    for hit in [None, Some(0), Some(8), Some(9), Some(40), Some(69)] {
        // Letters, and chars above 0x8030 (a 16-bit lane would read them as
        // negative after `- '0'`), and a digit at `hit`.
        let mut a: Vec<i64> = (0..len)
            .map(|i| {
                if i % 4 == 1 {
                    0x8031 + i as i64
                } else {
                    0x41 + (i % 26) as i64
                }
            })
            .collect();
        if let Some(p) = hit {
            a[p] = 0x37;
        }
        let buf = prim_array(2, &a);
        bufs.push((a, buf));
    }
    for (a, buf) in &bufs {
        for n in SCAN_NS {
            let want = (0..n).find(|&j| a[j] - 48 < 10).map_or(-1, |j| j as i64);
            cases.push((vec![buf.as_ptr() as i64, n as i64], want));
        }
    }
    check_scan_kernel("findDigit", "([CI)I", code, 3, 2, false, &cases);
}

/// L8-4: `while (j < n && a[j] < 0x80) j++;` over `char` lanes -- an
/// unsigned order compare (`0xFFFF` is not below `0x80`).
#[test]
fn wide_scan_char_order_compare_is_unsigned() {
    let code = vec![
        0x03, 0x3d, 0x1c, 0x1b, 0xa2, 0x00, 0x12, 0x2a, 0x1c, 0x34, 0x11, 0x00, 0x80, 0xa2, 0x00,
        0x09, 0x84, 0x02, 0x01, 0xa7, 0xff, 0xef, 0x1c, 0xac,
    ];
    let len = 70usize;
    let mut cases = Vec::new();
    let mut bufs = Vec::new();
    for (stop, v) in [
        (None, 0),
        (Some(0), 0x80),
        (Some(15), 0xFFFF),
        (Some(16), 0x8000),
        (Some(33), 0x7FFF),
        (Some(69), 0x81),
    ] {
        let mut a: Vec<i64> = (0..len).map(|i| (i % 0x80) as i64).collect();
        if let Some(p) = stop {
            a[p] = v;
        }
        let buf = prim_array(2, &a);
        bufs.push((a, buf));
    }
    for (a, buf) in &bufs {
        for n in SCAN_NS {
            let want = (0..n).find(|&j| a[j] >= 0x80).unwrap_or(n) as i64;
            cases.push((vec![buf.as_ptr() as i64, n as i64], want));
        }
    }
    check_scan_kernel("findAscii", "([CI)I", code, 3, 2, false, &cases);
}

/// L8-4, FP lanes: `a[j] == x` (`dcmpl; ifne`), `a[j] < x` (`dcmpg; ifge`)
/// over doubles and `a[j] != x` (`fcmpl; ifeq`) over floats, with NaN
/// elements and a NaN `x` (`==` never hits a NaN, `<` never does, `!=`
/// always does), and `-0.0 == +0.0`.
#[test]
fn wide_scan_fp_compares_follow_the_nan_rules() {
    let len = 37usize;
    let mut sets: Vec<Vec<f64>> = Vec::new();
    for nan_at in [None, Some(0), Some(5), Some(20)] {
        let mut v: Vec<f64> = (0..len).map(|i| i as f64 * 0.5 - 4.0).collect();
        if let Some(p) = nan_at {
            v[p] = f64::NAN;
        }
        v[30] = -0.0;
        sets.push(v);
    }
    let xs = [f64::NAN, 0.0, 3.5, -4.0, -100.0, 14.0, f64::INFINITY];
    let eq = vec![
        0x1f, 0xb8, 0x00, 0x04, 0x39, 0x04, 0x03, 0x36, 0x06, 0x15, 0x06, 0x1d, 0xa2, 0x00, 0x16,
        0x2a, 0x15, 0x06, 0x31, 0x18, 0x04, 0x97, 0x9a, 0x00, 0x06, 0x15, 0x06, 0xac, 0x84, 0x06,
        0x01, 0xa7, 0xff, 0xea, 0x02, 0xac,
    ];
    let lt = vec![
        0x1f, 0xb8, 0x00, 0x04, 0x39, 0x04, 0x03, 0x36, 0x06, 0x15, 0x06, 0x1d, 0xa2, 0x00, 0x16,
        0x2a, 0x15, 0x06, 0x31, 0x18, 0x04, 0x98, 0x9c, 0x00, 0x06, 0x15, 0x06, 0xac, 0x84, 0x06,
        0x01, 0xa7, 0xff, 0xea, 0x02, 0xac,
    ];
    let bufs: Vec<Vec<u8>> = sets
        .iter()
        .map(|v| prim_array(8, &v.iter().map(|x| x.to_bits() as i64).collect::<Vec<_>>()))
        .collect();
    let mut eq_cases = Vec::new();
    let mut lt_cases = Vec::new();
    for (v, buf) in sets.iter().zip(&bufs) {
        for &x in &xs {
            for n in [0usize, 1, 3, 4, 5, 16, 17, 21, 31, 36, 37] {
                let args = vec![buf.as_ptr() as i64, x.to_bits() as i64, n as i64];
                let fe = (0..n).find(|&j| v[j] == x).map_or(-1, |j| j as i64);
                let fl = (0..n).find(|&j| v[j] < x).map_or(-1, |j| j as i64);
                eq_cases.push((args.clone(), fe));
                lt_cases.push((args, fl));
            }
        }
    }
    check_scan_kernel("findD", "([DJI)I", eq, 7, 3, true, &eq_cases);
    check_scan_kernel("findLtD", "([DJI)I", lt, 7, 3, true, &lt_cases);

    let ne = vec![
        0x1b, 0xb8, 0x00, 0x05, 0x46, 0x03, 0x36, 0x04, 0x15, 0x04, 0x1c, 0xa2, 0x00, 0x15, 0x2a,
        0x15, 0x04, 0x30, 0x25, 0x95, 0x99, 0x00, 0x06, 0x15, 0x04, 0xac, 0x84, 0x04, 0x01, 0xa7,
        0xff, 0xeb, 0x02, 0xac,
    ];
    let mut fsets: Vec<Vec<f32>> = Vec::new();
    for odd_at in [None, Some(0), Some(7), Some(8), Some(36)] {
        let mut v = vec![2.5f32; len];
        v[3] = 2.5;
        if let Some(p) = odd_at {
            v[p] = if p == 7 { f32::NAN } else { -2.5 };
        }
        fsets.push(v);
    }
    let fbufs: Vec<Vec<u8>> = fsets
        .iter()
        .map(|v| {
            prim_array(
                4,
                &v.iter().map(|x| i64::from(x.to_bits())).collect::<Vec<_>>(),
            )
        })
        .collect();
    let mut ne_cases = Vec::new();
    for (v, buf) in fsets.iter().zip(&fbufs) {
        for x in [2.5f32, f32::NAN, -2.5] {
            for n in [0usize, 1, 7, 8, 9, 16, 17, 36, 37] {
                let want = (0..n).find(|&j| v[j] != x).map_or(-1, |j| j as i64);
                ne_cases.push((
                    vec![buf.as_ptr() as i64, i64::from(x.to_bits()), n as i64],
                    want,
                ));
            }
        }
    }
    check_scan_kernel("findNeF", "([FII)I", ne, 5, 3, true, &ne_cases);
}

/// L8-5 (BCE21-5 proper): `static int sumFrom3(int[] a) { int s = 0; for (int
/// j = 3; j != a.length; j++) s += a[j]; return s; }` -- the range-predicate
/// pass plants `3 <= a.length` in the pre-header and range BCE drops the
/// `a[j]` check under it. The answer is the same in both arms (lengths the
/// guard admits; a shorter array deopts, which this harness has no
/// interpreter for).
#[test]
fn bce_ne_exit_from_a_constant_start_keeps_the_answer() {
    let code = vec![
        0x03, 0x3c, 0x06, 0x3d, 0x1c, 0x2a, 0xbe, 0x9f, 0x00, 0x0f, 0x1b, 0x2a, 0x1c, 0x2e, 0x60,
        0x3c, 0x84, 0x02, 0x01, 0xa7, 0xff, 0xf1, 0x1b, 0xac,
    ];
    let h = helpers();
    let cm = cached("sumFrom3", "([I)I", code, 3, 1);
    for arm in [Some("0"), None] {
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_IR_BCE_NE_EXIT_START", arm),
                ("CRATONVM_JIT_IR_VECTOR_LOOP", Some("0")),
            ],
            || {
                let m = compile(&cm, &h, None, false);
                for len in [3usize, 4, 5, 9, 40] {
                    let a: Vec<i64> = (0..len).map(|i| i as i64 * 11 - 7).collect();
                    let arr = prim_array(4, &a);
                    let got = call(&m, &[arr.as_ptr() as i64]);
                    let want = a[3..].iter().fold(0i32, |s, &x| s.wrapping_add(x as i32));
                    assert_eq!(got as i32, want, "arm={arm:?} len={len}");
                }
            },
        );
    }
}
