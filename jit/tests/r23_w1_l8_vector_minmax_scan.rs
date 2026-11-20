// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
/// VEC21-8 with a miss mask: `static int skip(int[] a, int c, int n) { int j
/// = 0; while (j < n && a[j] < c) j++; return j; }` -- the loop leaves on
/// `a[j] >= c`, which the pre-loop tests as NOT `c > a[j]`.
/// VEC21-5: `static int imax(int[] a, int m, int n) { for (int j = 0; j < n;
/// j++) m = Math.max(m, a[j]); return m; }` -- MIN/MAX values, odd lengths.
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

// VEC21-5, FP: `static long dmax(double[] a, long seed, int n) { double m =
// Double.longBitsToDouble(seed); for (int j = 0; j < n; j++) m =
// Math.max(m, a[j]); return Double.doubleToRawLongBits(m); }` under
// `CRATONVM_JIT_IR_FP_MATH_OPS=1` (the FP min/max sites' own opt-in). The
// raw-bits pair keeps every argument and the answer in GPRs -- the shape the
// VM's own `R23W1L8VecRows.dmax` admits (the w1 form, which read and wrote
// the seed through a `double[]`, fell back to single-pass in this harness).
// Java-exact bits: `-0.0` vs `+0.0` in every lane position, NaNs with
// distinct payloads (the FIRST one in program order wins), a NaN seed.


// BCE21-5's vector side: `static int sum(int[] a, int n) { int s = 0; for
// (int j = 0; j != n; j++) s += a[j]; return s; }` -- a `!=` loop test is
// admitted like `<` (the pre-loop steps only while `j + 8 <= n`).
// BCE21-5 (r23 wave 2): `static int sumNe(int[] a) { int s = 0; for (int j =
// 0; j != a.length; j++) s += a[j]; return s; }` -- with
// `CRATONVM_JIT_IR_BCE_NE_EXIT` the `a[j]` bounds check goes (the body is
// smaller); the answer is the same in both arms. The vector pre-loop is off
// in both, so the size difference is the check alone.
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

// L8-5 (BCE21-5 proper): `static int sumFrom3(int[] a) { int s = 0; for (int
// j = 3; j != a.length; j++) s += a[j]; return s; }` -- the range-predicate
// pass plants `3 <= a.length` in the pre-header and range BCE drops the
// `a[j]` check under it. The answer is the same in both arms (lengths the
// guard admits; a shorter array deopts, which this harness has no
// interpreter for).
