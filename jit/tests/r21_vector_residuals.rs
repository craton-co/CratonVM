// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
/// VEC21-2 + the `long` lane multiply: `static void f(long[] d, X[] a, long
/// k, int n) { for (int j = 0; j < n; j++) d[j] = a[j] * k; }` for `X` each
/// of `int`, `char`, `short`, `byte` (`i2l` folded into the widening load) --
/// `k` near the 64-bit edges, so every partial product of the three
/// `VPMULUDQ` matters.
/// VEC21-2, FP: `static void f(double[] d, float[] f, double k, int n) {
/// for (int j = 0; j < n; j++) d[j] = f[j] * k; }` -- `VCVTPS2PD` is `f2d`
/// lane by lane: NaNs (payload included), `-0.0`, subnormals, infinities.
/// VEC21-2 through a reduction: `static int f(X[] a, int n) { int s = 0; for
/// (int j = 0; j < n; j++) s += a[j]; return s; }` for `byte[]` and
/// `char[]` -- the lanes are the accumulator's `int`.
/// VEC21-6: `static void f(int[] c, int[] a, int k, int n) { for (int j = 0;
/// j < n; j++) c[j] = ((a[j] << k) ^ (a[j] >>> k)) ^ (a[j] >> k); }` -- one
/// count register for the three shifts; `k` over every masking corner (the
/// x86 count form zeroes a lane for a count >= 32, Java masks `k & 31`).
/// VEC21-6, `long` lanes: `static void f(long[] c, long[] a, int k, int n) {
/// for (int j = 0; j < n; j++) c[j] = (a[j] << k) + (a[j] >>> k); }` --
/// `k & 63` (`>>` has no AVX2 form on `long` and stays refused).
/// VEC21-7: `static void f(int[] c, int[] a, int[] b, int k, int m, int n) {
/// for (int j = 0; j < n; j++) c[j] = a[j] * k + b[j] * m; }` -- two
/// invariants and two temporaries, four vector registers: Windows used to
/// refuse it (its pool was XMM0/XMM1); with VEC21-7 a body with a vector plan
/// saves XMM6-XMM11 in its prologue, so the pre-loop has XMM8-XMM11 there too.
/// The switch moves nothing on System V. A Windows caller's XMM8-XMM15 must
/// come back unchanged: every call goes through a trampoline that seeds them
/// and checks them after the call.
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

// VEC21-7 for VEC21-5's register-hungry arms (r23 L8-2): `static long
// lmax(long[] a, long m, int n) { for (int j = 0; j < n; j++) m =
// Math.max(m, a[j]); return m; }` -- a `long` min/max step holds the
// accumulator and two temporaries, more than Windows' XMM0/XMM1.
// VEC21-2, narrowing: `static void f(T[] d, int[] a, int k, int n) { for
// (int j = 0; j < n; j++) d[j] = (T) (a[j] * k); }` for `T` each of `byte`,
// `char`, `short` -- `int` lanes truncated at the store, as
// `bastore`/`castore`/`sastore` truncate. The `byte` arm also stores into a
// `boolean[]`, which keeps `value & 1` (JVMS 6.5): the scalar loop's.
// VEC21-2, narrowing with the destination also read (widened):
// `static void f(byte[] b, int[] a, int n) { for (int j = 0; j < n; j++)
// b[j] = (byte) (b[j] + a[j]); }` -- every lane is read before its store.
