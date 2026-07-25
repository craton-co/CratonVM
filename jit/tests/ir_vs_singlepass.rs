// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! activate-ir-optimizer step 2 — IR-vs-single-pass differential self-check.
//!
//! The optimizing IR pipeline and the single-pass `x64` backend are two
//! independent code generators for the same bytecode. Before the IR gate is
//! relaxed to run on more method shapes (step 3), they must be proven to agree:
//! a divergence is a miscompile. This harness compiles a corpus of pure-integer
//! methods through BOTH backends via the per-call `optimize` toggle
//! (`try_compile(.., optimize=true)` = IR pipeline, `optimize=false` =
//! single-pass) and asserts they return identical results for every sample
//! input — plus a host-computed correctness anchor so a *shared* bug is caught
//! too.
//!
//! Pure-integer methods are used deliberately: the IR pipeline only handles
//! 32-bit-int, call-free code (category-2 long/double and `invoke*` methods fall
//! back to single-pass, making the comparison trivially identical), so an int
//! corpus is exactly where the two backends genuinely differ.

use cratonvm_jit::{try_compile, CachedBytecodeMethod, CompiledMethod, JitRuntimeHelpers};
use cratonvm_types::{
    ClassId, ARRAY_LENGTH_OFFSET, FIELD_CELL_PAYLOAD32_OFFSET, HEADER_SIZE, SLOT_SIZE,
};
use std::sync::Arc;

/// Dummy runtime helpers — the corpus is pure arithmetic / branches / counted
/// loops, so no helper (alloc, field, dispatch) is ever invoked; the stub
/// pointer is baked but never called.
/// Wide-open region bounds table for the guarded inline getfield: one region
/// spanning [0x1000, usize::MAX) so every 8-aligned non-null test object
/// passes the inline receiver guard and is raw-loaded (no helper call) —
/// the same "no runtime helper reachable" contract these tests were written
/// against when raw-inline getfield was the default.
static TEST_REGION_BOUNDS: [std::sync::atomic::AtomicUsize; 6] = [
    std::sync::atomic::AtomicUsize::new(0x1000),
    std::sync::atomic::AtomicUsize::new(usize::MAX),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
];

fn dummy_helpers() -> JitRuntimeHelpers {
    // guarded_inline_getfield_enabled() is default-ON (jit/src/x64.rs) --
    // this whole file's contract ("no runtime helper reachable" via the
    // wide-open TEST_REGION_BOUNDS above) relies on the guarded-inline path
    // being taken, which now happens without an explicit opt-in.
    unsafe extern "C" fn stub() {
        panic!("ir_vs_singlepass invoked an unwired runtime helper");
    }
    unsafe extern "C" fn self_guard(_vm_ptr: i64) -> i64 {
        0
    }
    unsafe extern "C" fn native_stack_floor() -> i64 {
        0
    }
    let s = stub as *const () as usize;
    JitRuntimeHelpers { safepoint_flag_addr: 0, safepoint_slow_path: 0,
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
        // Panic stub by default: only consulted on a J/D call's `RAX == i64::MIN`
        // branch, which no existing test reaches. The long-return tests below
        // override this per-test with a real returns-0/1 stub.
        dispatch_threw: s,
        jit_frem: s,
        jit_drem: s,
        self_call_stack_guard: self_guard as *const () as usize,
        region_bounds_addr: TEST_REGION_BOUNDS.as_ptr() as usize,
        native_stack_floor_fn: native_stack_floor as *const () as usize,
        ldc_string: s,
    }
}

fn cached(
    name: &str,
    descriptor: &str,
    mut code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
) -> CachedBytecodeMethod {
    // VM convention: `CachedBytecodeMethod.code` is the bytecode padded with two
    // trailing 0x00 bytes (`vm/src/.../interpreter.rs` "padded_bytecode adds 2";
    // `frame.rs` asserts the two trailing zeros). `jit::try_compile` recovers the
    // real length via `code.len() - 2`. Without the padding the last two opcodes
    // are dropped — the method emits without a `ret` and crashes when called.
    code.push(0x00);
    code.push(0x00);
    CachedBytecodeMethod {
        declaring_class_id: ClassId::new(1),
        class_name: Arc::from("Corpus"),
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
        native_callback_cache: std::sync::OnceLock::new(),
    }
}

fn compile_opt(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    optimize: bool,
) -> Option<CompiledMethod> {
    try_compile(
        cm, None, None, None, None, None, None, None, None, None, helpers, None, None, None, None,
        optimize, false, false, false, false, false, None,
    )
}

/// Compile `(name, code)` both ways and assert IR == single-pass == `expected`
/// (low 32 bits — an int method) for every `(args, expected)` case.
fn check(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
    cases: &[(Vec<i64>, i32)],
) {
    let helpers = dummy_helpers();
    let cm = cached(name, descriptor, code, max_locals, num_params);
    let ir = compile_opt(&cm, &helpers, true)
        .unwrap_or_else(|| panic!("{name}: optimize=true (IR pipeline) failed to compile"));
    let sp = compile_opt(&cm, &helpers, false)
        .unwrap_or_else(|| panic!("{name}: optimize=false (single-pass) failed to compile"));
    for (args, expected) in cases {
        // SAFETY: both bodies were produced by the JIT from valid pure-int
        // bytecode into executable memory; the System V i64-arg / i64-ret entry
        // ABI matches `try_call` (the same path the VM uses to invoke both
        // backends), and no runtime helper is reachable for these methods.
        let r_sp = unsafe { sp.try_call(args) }
            .unwrap_or_else(|e| panic!("{name}: single-pass call {args:?}: {e:?}"));
        let r_ir = unsafe { ir.try_call(args) }
            .unwrap_or_else(|e| panic!("{name}: IR call {args:?}: {e:?}"));
        assert_eq!(
            r_ir as i32, r_sp as i32,
            "{name}: IR vs single-pass DIVERGE for {args:?}: IR={}, single-pass={}",
            r_ir as i32, r_sp as i32,
        );
        assert_eq!(
            r_ir as i32, *expected,
            "{name}: both backends agree but disagree with host for {args:?}: got {}, expected {expected}",
            r_ir as i32,
        );
    }
}

/// Like [`compile_opt`] but with the `ir_emit_long` gate ON (inc 25), so the IR
/// pipeline admits long-using methods. The single-pass side is unaffected
/// (`ir_emit_long` is consulted only on the optimizing path).
fn compile_long_opt(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    optimize: bool,
) -> Option<CompiledMethod> {
    try_compile(
        cm, None, None, None, None, None, None, None, None, None, helpers, None, None, None, None,
        optimize, false, false, true, false, false, None,
    )
}

/// Like [`compile_opt`] but with the `ir_emit_fp` gate ON (inc 30), so the IR
/// pipeline admits a method that uses `float`/`double` internally (FP-free
/// signature). The single-pass side is unaffected.
fn compile_fp_opt(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    optimize: bool,
) -> Option<CompiledMethod> {
    try_compile(
        cm, None, None, None, None, None, None, None, None, None, helpers, None, None, None, None,
        optimize, false, false, false, false, true, None,
    )
}

/// Compile an FP-using `(name, code)` both ways (IR with `ir_emit_fp` on vs
/// single-pass) and assert IR == single-pass == `expected` (low 32 bits — every
/// FP corpus method takes int args and returns an int, so the GPR i64-arg /
/// i64-ret `try_call` ABI is exact and the FP work stays internal to XMM). The
/// host-computed `expected` is the IEEE-754 anchor catching a *shared* bug.
fn check_fp(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
    cases: &[(Vec<i64>, i32)],
) {
    let helpers = dummy_helpers();
    let cm = cached(name, descriptor, code, max_locals, num_params);
    let ir = compile_fp_opt(&cm, &helpers, true)
        .unwrap_or_else(|| panic!("{name}: optimize=true (IR, FP) failed to compile"));
    let sp = compile_fp_opt(&cm, &helpers, false)
        .unwrap_or_else(|| panic!("{name}: optimize=false (single-pass) failed to compile"));
    for (args, expected) in cases {
        // SAFETY: both bodies were produced by the JIT from valid FP bytecode
        // with an int signature; the i64-arg / i64-ret ABI matches `try_call`,
        // no helper is reachable.
        let r_sp = unsafe { sp.try_call(args) }
            .unwrap_or_else(|e| panic!("{name}: single-pass call {args:?}: {e:?}"));
        let r_ir = unsafe { ir.try_call(args) }
            .unwrap_or_else(|e| panic!("{name}: IR call {args:?}: {e:?}"));
        assert_eq!(
            r_ir as i32, r_sp as i32,
            "{name}: IR vs single-pass DIVERGE for {args:?}: IR={}, single-pass={}",
            r_ir as i32, r_sp as i32,
        );
        assert_eq!(
            r_ir as i32, *expected,
            "{name}: both backends agree but disagree with host for {args:?}: got {}, expected {expected}",
            r_ir as i32,
        );
    }
}

/// Compile a long-using `(name, code)` both ways and assert IR == single-pass ==
/// `expected` over the FULL 64 bits (a `long` method). The JIT ABI passes each
/// parameter as one i64 register and returns the result in RAX, so `try_call`
/// args/return are full i64. (Routing through the IR path — not a vacuous
/// single-pass fall-through — is separately proven by `ir_long_wiring_…` in
/// `lib.rs` via `IR_LOWER_COMPILES`.)
fn check_long(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
    cases: &[(Vec<i64>, i64)],
) {
    let helpers = dummy_helpers();
    let cm = cached(name, descriptor, code, max_locals, num_params);
    let ir = compile_long_opt(&cm, &helpers, true)
        .unwrap_or_else(|| panic!("{name}: optimize=true (IR, long) failed to compile"));
    let sp = compile_long_opt(&cm, &helpers, false)
        .unwrap_or_else(|| panic!("{name}: optimize=false (single-pass) failed to compile"));
    for (args, expected) in cases {
        // SAFETY: both bodies were produced by the JIT from valid long bytecode;
        // the i64-arg / i64-ret ABI matches `try_call`, no helper is reachable.
        let r_sp = unsafe { sp.try_call(args) }
            .unwrap_or_else(|e| panic!("{name}: single-pass call {args:?}: {e:?}"));
        let r_ir = unsafe { ir.try_call(args) }
            .unwrap_or_else(|e| panic!("{name}: IR call {args:?}: {e:?}"));
        assert_eq!(
            r_ir, r_sp,
            "{name}: IR vs single-pass DIVERGE (full i64) for {args:?}: IR={r_ir}, single-pass={r_sp}",
        );
        assert_eq!(
            r_ir, *expected,
            "{name}: both backends agree but disagree with host for {args:?}: got {r_ir}, expected {expected}",
        );
    }
}

#[test]
fn ir_vs_singlepass_long_add() {
    // long add(long a, long b) { return a + b; }
    //   lload_0; lload_2; ladd; lreturn
    let code = vec![0x1e, 0x20, 0x61, 0xad];
    check_long(
        "ladd",
        "(JJ)J",
        code,
        4,
        2,
        &[
            (vec![3, 4], 7),
            (vec![i64::MAX, 1], i64::MIN), // 64-bit wrap
            // genuinely 64-bit (a 32-bit ADD would drop the high word):
            (vec![0x1_0000_0000, 0x2_0000_0000], 0x3_0000_0000),
            (vec![-5, -7], -12),
        ],
    );
}

#[test]
fn ir_vs_singlepass_long_two_slot_params() {
    // long f(long a, long b) { return a*b - b; }  — exercises lload_2 (b at JVM
    // slot 2, the heart of the cat-2 two-slot param layout fix).
    //   lload_0; lload_2; lmul; lload_2; lsub; lreturn
    let code = vec![0x1e, 0x20, 0x69, 0x20, 0x65, 0xad];
    check_long(
        "lmulsub",
        "(JJ)J",
        code,
        4,
        2,
        &[
            (vec![3, 4], 8),                             // 12 - 4
            (vec![0x1_0000_0000, 3], 0x3_0000_0000 - 3), // 64-bit mul
            (vec![-2, 5], -15),                          // -10 - 5
        ],
    );
}

#[test]
fn ir_vs_singlepass_long_mixed_int_long_param() {
    // long f(int a, long b) { return (long)a + b; }  — int param `a` at slot 0
    // (1 slot), long `b` at slots 1-2; lload_1 must read b.
    //   iload_0; i2l; lload_1; ladd; lreturn
    let code = vec![0x1a, 0x85, 0x1f, 0x61, 0xad];
    check_long(
        "mixedil",
        "(IJ)J",
        code,
        3,
        2,
        &[
            (vec![5, 7], 12),
            (vec![-1, 0x1_0000_0000], 0x1_0000_0000 - 1), // i2l sign-extends a
            (vec![100, -50], 50),
        ],
    );
}

#[test]
fn ir_vs_singlepass_long_to_int_return() {
    // int f(long a, long b) { return (int)(a + b); }  — l2i truncates to low 32.
    //   lload_0; lload_2; ladd; l2i; ireturn
    let code = vec![0x1e, 0x20, 0x61, 0x88, 0xac];
    let helpers = dummy_helpers();
    let cm = cached("l2iret", "(JJ)I", code, 4, 2);
    let ir = compile_long_opt(&cm, &helpers, true).expect("IR long");
    let sp = compile_long_opt(&cm, &helpers, false).expect("single-pass");
    for (a, b) in [(3i64, 4i64), (0x1_0000_0005, 0x1_0000_0002), (i64::MAX, 1)] {
        let r_ir = unsafe { ir.try_call(&[a, b]) }.unwrap();
        let r_sp = unsafe { sp.try_call(&[a, b]) }.unwrap();
        assert_eq!(
            r_ir as i32, r_sp as i32,
            "l2i IR vs single-pass for ({a},{b})"
        );
        assert_eq!(
            r_ir as i32,
            a.wrapping_add(b) as i32,
            "l2i vs host for ({a},{b})"
        );
    }
}

#[test]
fn ir_vs_singlepass_long_wide_load_store() {
    // inc 26: long params/locals at JVM slots >= 4 use the WIDE `lload` (0x16) /
    // `lstore` (0x37) forms (not the short `lload_2`/`lstore_3`).
    // long f(long a, long b, long c) { long d = a + b; long e = c - d; return d * e; }
    //   a@0-1 b@2-3 c@4-5 d@6-7 e@8-9
    //   lload_0; lload_2; ladd; lstore 6; lload 4; lload 6; lsub; lstore 8;
    //   lload 6; lload 8; lmul; lreturn
    let code = vec![
        0x1e, 0x20, 0x61, // d = a + b
        0x37, 0x06, // lstore 6 (d)
        0x16, 0x04, // lload 4 (c)
        0x16, 0x06, // lload 6 (d)
        0x65, // lsub  -> c - d
        0x37, 0x08, // lstore 8 (e)
        0x16, 0x06, // lload 6 (d)
        0x16, 0x08, // lload 8 (e)
        0x69, // lmul -> d * e
        0xad, // lreturn
    ];
    check_long(
        "lwide",
        "(JJJ)J",
        code,
        10,
        3,
        &[
            // host uses wrapping ops to match the JVM's 64-bit `long` semantics.
            (vec![3, 4, 100], {
                let d = 3i64.wrapping_add(4);
                let e = 100i64.wrapping_sub(d);
                d.wrapping_mul(e) // 7 * 93 = 651
            }),
            (vec![0x1_0000_0000, 1, 0x4_0000_0000], {
                let d = 0x1_0000_0000i64.wrapping_add(1);
                let e = 0x4_0000_0000i64.wrapping_sub(d);
                d.wrapping_mul(e) // overflows 64-bit → wraps
            }),
            (vec![-2, -3, 10], {
                let d = (-2i64).wrapping_add(-3);
                let e = 10i64.wrapping_sub(d);
                d.wrapping_mul(e)
            }),
        ],
    );
}

/// Like [`compile_long_opt`] but also supplies an `ldc2_w` long-constant resolver
/// (inc 26) so the IR builder can lower long constants from the constant pool.
fn compile_long_ldc2w(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    optimize: bool,
    ldc2w: &dyn Fn(u16) -> Option<(i64, bool)>, // inc 35: (bits, is_double)
) -> Option<CompiledMethod> {
    try_compile(
        cm,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(ldc2w),
        None,
        helpers,
        None,
        None,
        None,
        None,
        optimize,
        false,
        false,
        true,
        false, // ir_emit_virtual_calls
        false, // ir_emit_fp
        None,
    )
}

/// Like [`compile_long_ldc2w`] but with the FP gate on (inc 35) so a `double`
/// `ldc2_w` constant is admitted and lowered to `dconst`.
fn compile_fp_ldc2w(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    optimize: bool,
    ldc2w: &dyn Fn(u16) -> Option<(i64, bool)>,
) -> Option<CompiledMethod> {
    try_compile(
        cm,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(ldc2w),
        None,
        helpers,
        None,
        None,
        None,
        None,
        optimize,
        false,
        false,
        true,
        false,
        true, // ir_emit_fp ON
        None,
    )
}

#[test]
fn ir_vs_singlepass_double_ldc2w_constant() {
    // inc 35: a `double` `ldc2_w` constant. double f(double a) { return a * 1.5; }
    //   dload_0; ldc2_w #1 (1.5); dmul; dreturn
    const C: f64 = 1.5;
    let code = vec![0x26, 0x14, 0x00, 0x01, 0x6b, 0xaf];
    let ldc2w = |cp: u16| -> Option<(i64, bool)> {
        match cp {
            1 => Some((C.to_bits() as i64, true)), // double → is_double = true
            _ => None,
        }
    };
    let helpers = dummy_helpers();
    let cm = cached("dldc", "(D)D", code, 2, 1);
    let ir = compile_fp_ldc2w(&cm, &helpers, true, &ldc2w).expect("IR double ldc2_w");
    let sp = compile_fp_ldc2w(&cm, &helpers, false, &ldc2w).expect("single-pass double ldc2_w");
    for a in [3.0f64, 0.0, -7.25, 1e10, -0.5] {
        let args = [a.to_bits() as i64];
        let r_ir = f64::from_bits(unsafe { ir.try_call(&args) }.unwrap() as u64);
        let r_sp = f64::from_bits(unsafe { sp.try_call(&args) }.unwrap() as u64);
        let host = a * C;
        assert_eq!(r_ir.to_bits(), host.to_bits(), "double ldc2_w IR for a={a}");
        assert_eq!(
            r_sp.to_bits(),
            host.to_bits(),
            "double ldc2_w single-pass for a={a}"
        );
    }
}

#[test]
fn ir_vs_singlepass_long_ldc2w_constant() {
    // inc 26: `ldc2_w` long constants. long f(long a) { return a * C1 + C2; }
    //   lload_0; ldc2_w #1; lmul; ldc2_w #2; ladd; lreturn
    const C1: i64 = 1_000_000_007;
    const C2: i64 = 0x7FFF_FFFF_0000_002A; // a large 64-bit constant (not 32-bit)
    let code = vec![
        0x1e, // lload_0 (a)
        0x14, 0x00, 0x01, // ldc2_w #1 (C1)
        0x69, // lmul
        0x14, 0x00, 0x02, // ldc2_w #2 (C2)
        0x61, // ladd
        0xad, // lreturn
    ];
    let ldc2w = |cp: u16| -> Option<(i64, bool)> {
        match cp {
            1 => Some((C1, false)), // long constants → is_double = false
            2 => Some((C2, false)),
            _ => None,
        }
    };
    let helpers = dummy_helpers();
    let cm = cached("lldc", "(J)J", code, 2, 1);
    let ir = compile_long_ldc2w(&cm, &helpers, true, &ldc2w).expect("IR long ldc2_w");
    let sp = compile_long_ldc2w(&cm, &helpers, false, &ldc2w).expect("single-pass");
    for a in [3i64, 0, -7, 0x1_0000_0000, i64::MAX] {
        let r_ir = unsafe { ir.try_call(&[a]) }.unwrap();
        let r_sp = unsafe { sp.try_call(&[a]) }.unwrap();
        let host = a.wrapping_mul(C1).wrapping_add(C2);
        assert_eq!(r_ir, r_sp, "ldc2_w IR vs single-pass for a={a}");
        assert_eq!(r_ir, host, "ldc2_w vs host for a={a}");
    }
}

#[test]
fn ir_vs_singlepass_long_ldiv() {
    // long signed division. long f(long a, long b) { return a / b; }
    //   lload_0; lload_2; ldiv; lreturn
    // Non-zero divisors only — a zero divisor deopts (returns the i64::MIN
    // sentinel) and is validated live (the interpreter throws ArithmeticException).
    let code = vec![0x1e, 0x20, 0x6d, 0xad];
    check_long(
        "ldiv",
        "(JJ)J",
        code,
        4,
        2,
        &[
            (vec![7, 2], 3),
            (vec![-7, 2], -3), // Java truncates toward zero
            (vec![7, -2], -3),
            (vec![-7, -2], 3),
            // genuinely 64-bit: a 32-bit IDIV would mis-divide these.
            (vec![0x7FFF_FFFF_FFFF_FFFF, 3], 0x7FFF_FFFF_FFFF_FFFF / 3),
            (vec![0x1_0000_0000, 2], 0x8000_0000),
            // JVMS §6.5.ldiv overflow: LONG_MIN / -1 == LONG_MIN (no #DE / no
            // exception). The lowerer's overflow guard must synthesise this.
            (vec![i64::MIN, -1], i64::MIN),
        ],
    );
}

#[test]
fn ir_vs_singlepass_long_lrem() {
    // long signed remainder (mirrors ldiv). long f(long a,long b){return a%b;}
    //   lload_0; lload_2; lrem; lreturn
    let code = vec![0x1e, 0x20, 0x71, 0xad];
    check_long(
        "lrem",
        "(JJ)J",
        code,
        4,
        2,
        &[
            (vec![7, 2], 1),
            (vec![-7, 2], -1), // Java: remainder sign follows the dividend
            (vec![7, -2], 1),
            (vec![-7, -2], -1),
            (
                vec![0x7FFF_FFFF_FFFF_FFFF, 1_000_000_007],
                0x7FFF_FFFF_FFFF_FFFF % 1_000_000_007,
            ),
            // JVMS §6.5.lrem overflow: LONG_MIN % -1 == 0 (no #DE / no exception).
            (vec![i64::MIN, -1], 0),
        ],
    );
}

#[test]
fn ir_vs_singlepass_long_shifts() {
    // inc 27: long shifts. long f(long a, int n){ return (a<<n) + (a>>n) + (a>>>n); }
    //   a@0-1, n@2.  lload_0; iload_2; lshl; lload_0; iload_2; lshr; ladd;
    //   lload_0; iload_2; lushr; ladd; lreturn
    let code = vec![
        0x1e, 0x1c, 0x79, // a << n
        0x1e, 0x1c, 0x7b, 0x61, // + (a >> n)
        0x1e, 0x1c, 0x7d, 0x61, // + (a >>> n)
        0xad,
    ];
    check_long(
        "lshifts",
        "(JI)J",
        code,
        3,
        2,
        &[
            // host: JVM masks the shift count to 6 bits (count & 0x3f) for long.
            (vec![1, 1], {
                let a = 1i64;
                (a << (1 & 63))
                    .wrapping_add(a >> (1 & 63))
                    .wrapping_add((a as u64 >> (1 & 63)) as i64)
            }),
            (vec![-1, 4], {
                let a = -1i64;
                (a << 4)
                    .wrapping_add(a >> 4)
                    .wrapping_add((a as u64 >> 4) as i64)
            }),
            (vec![0x1234_5678_9abc_def0u64 as i64, 40], {
                let a = 0x1234_5678_9abc_def0u64 as i64;
                (a << (40 & 63))
                    .wrapping_add(a >> (40 & 63))
                    .wrapping_add((a as u64 >> (40 & 63)) as i64)
            }),
            (vec![i64::MIN, 1], {
                let a = i64::MIN;
                (a << 1)
                    .wrapping_add(a >> 1)
                    .wrapping_add((a as u64 >> 1) as i64)
            }),
        ],
    );
}

#[test]
fn ir_vs_singlepass_long_bitwise() {
    // inc 27: long bitwise. long f(long a, long b){ return (a & b) | (a ^ b); }
    //   lload_0; lload_2; land; lload_0; lload_2; lxor; lor; lreturn
    let code = vec![
        0x1e, 0x20, 0x7f, // a & b
        0x1e, 0x20, 0x83, // a ^ b
        0x81, // |
        0xad,
    ];
    check_long(
        "lbitwise",
        "(JJ)J",
        code,
        4,
        2,
        &[
            (
                vec![
                    0x0f0f_0f0f_0f0f_0f0fu64 as i64,
                    0x00ff_00ff_00ff_00ffu64 as i64,
                ],
                {
                    let (a, b) = (
                        0x0f0f_0f0f_0f0f_0f0fu64 as i64,
                        0x00ff_00ff_00ff_00ffu64 as i64,
                    );
                    (a & b) | (a ^ b)
                },
            ),
            (vec![-1, 0], -1),
            (vec![0, -1], -1),
            (vec![i64::MIN, i64::MAX], {
                let (a, b) = (i64::MIN, i64::MAX);
                (a & b) | (a ^ b)
            }),
        ],
    );
}

#[test]
fn ir_vs_singlepass_long_lcmp_branch() {
    // inc 27: lcmp + a long-fed branch. long min(long a, long b){ return a<b?a:b; }
    //   lload_0; lload_2; lcmp; ifge ELSE; lload_0; lreturn; ELSE: lload_2; lreturn
    //   pc0 lload_0; pc1 lload_2; pc2 lcmp; pc3 ifge +5(->pc8);
    //   pc6 lload_0; pc7 lreturn; pc8 lload_2; pc9 lreturn
    let code = vec![
        0x1e, 0x20, 0x94, // a, b, lcmp
        0x9c, 0x00, 0x05, // ifge +5 -> pc 8 (else: a >= b -> return b)
        0x1e, 0xad, // then (a < b): return a
        0x20, 0xad, // else: return b
    ];
    check_long(
        "lmin",
        "(JJ)J",
        code,
        4,
        2,
        &[
            (vec![3, 4], 3),
            (vec![4, 3], 3),
            (vec![5, 5], 5),
            (vec![-1, i64::MIN], i64::MIN),
            (vec![i64::MAX, i64::MIN], i64::MIN),
            (vec![0x1_0000_0000, 0xffff_ffff], 0xffff_ffff),
        ],
    );
}

#[test]
fn ir_vs_singlepass_long_loop_phi_lcmp() {
    // inc 27: a long loop-carried accumulator with a long-fed loop condition —
    // exercises Op::LCmp + a backward long branch + a long loop phi (now typed
    // Long, not Int) + wide lload/lstore, all together. No int counter (avoids
    // the not-yet-handled wide iload), no idiv/call (stays safepoint-free).
    //   long f(long a, long limit){ long s=0; while (s < limit) s = s + a; return s; }
    //   a@0-1, limit@2-3, s@4-5
    let code = vec![
        0x09, // lconst_0
        0x37, 0x04, // lstore 4 (s=0)
        // LOOP (pc 3):
        0x16, 0x04, // lload 4 (s)
        0x20, // lload_2 (limit)
        0x94, // lcmp
        0x9c, 0x00, 0x0c, // ifge +12 -> END (pc 19) if s >= limit
        0x16, 0x04, // lload 4 (s)
        0x1e, // lload_0 (a)
        0x61, // ladd
        0x37, 0x04, // lstore 4 (s = s + a)
        0xa7, 0xff, 0xf3, // goto -13 -> LOOP (pc 3)
        // END (pc 19):
        0x16, 0x04, // lload 4 (s)
        0xad, // lreturn
    ];
    check_long(
        "lloop",
        "(JJ)J",
        code,
        6,
        2,
        &[
            (vec![3, 10], {
                let (a, limit) = (3i64, 10i64);
                let mut s = 0i64;
                while s < limit {
                    s = s.wrapping_add(a);
                }
                s
            }),
            (vec![5, 5], 5),
            (vec![1, 64], 64),
            (vec![7, 50], 56),
            (vec![100, 1], 100),
        ],
    );
}

#[test]
fn ir_vs_singlepass_add() {
    // int add(int a, int b) { return a + b; }
    //   iload_0; iload_1; iadd; ireturn
    check(
        "add",
        "(II)I",
        vec![0x1a, 0x1b, 0x60, 0xac],
        2,
        2,
        &[
            (vec![3, 4], 7),
            (vec![10, -3], 7),
            (vec![-5, -6], -11),
            (vec![i32::MAX as i64, 1], i32::MIN), // wraps
        ],
    );
}

#[test]
fn ir_vs_singlepass_poly() {
    // int poly(int a) { return a*a - 2*a + 1; }   (== (a-1)^2)
    //   iload_0; iload_0; imul; iconst_2; iload_0; imul; isub; iconst_1; iadd; ireturn
    check(
        "poly",
        "(I)I",
        vec![
            0x1a, 0x1a, 0x68, // a*a
            0x05, 0x1a, 0x68, // 2*a
            0x64, // a*a - 2*a
            0x04, 0x60, // + 1
            0xac,
        ],
        1,
        1,
        &[
            (vec![1], 0),
            (vec![3], 4),
            (vec![0], 1),
            (vec![-2], 9),
            (vec![5], 16),
        ],
    );
}

// ── Multiple return points (conditional early return) ──────────────────
//
// These were the harness's first catch: the IR pipeline used to root DCE only
// from `graph.exit` (the LAST `Op::Return`, since each `ireturn` overwrites it),
// deleting every other return path and collapsing the conditional into a
// single-successor branch that always took the surviving return. Fixed by
// rooting `eliminate_dead_nodes` from ALL `Op::Return` nodes (ir_optimize.rs).

#[test]
fn ir_vs_singlepass_conditional_early_return() {
    // int sgn2(int a) { if (a<0) return -1; return 1; }   (one branch, two returns)
    check(
        "sgn2",
        "(I)I",
        vec![
            0x1a, 0x9c, 0x00, 0x05, // iload_0; ifge +5 → 6
            0x02, 0xac, // iconst_m1; ireturn
            0x04, 0xac, // iconst_1; ireturn
        ],
        1,
        1,
        &[
            (vec![-5], -1),
            (vec![0], 1),
            (vec![7], 1),
            (vec![i32::MIN as i64], -1),
            (vec![i32::MAX as i64], 1),
        ],
    );
}

#[test]
fn ir_vs_singlepass_two_branch_three_returns() {
    // int sgn3(int a) { if (a<0) return -1; if (a>0) return 1; return 0; }
    check(
        "sgn3",
        "(I)I",
        vec![
            0x1a, 0x9c, 0x00, 0x05, // iload_0; ifge +5 → 6
            0x02, 0xac, // iconst_m1; ireturn
            0x1a, 0x9e, 0x00, 0x05, // iload_0; ifle +5 → 12
            0x04, 0xac, // iconst_1; ireturn
            0x03, 0xac, // iconst_0; ireturn
        ],
        1,
        1,
        &[
            (vec![-5], -1),
            (vec![0], 0),
            (vec![7], 1),
            (vec![i32::MIN as i64], -1),
            (vec![i32::MAX as i64], 1),
        ],
    );
}

#[test]
fn ir_vs_singlepass_abs_early_return() {
    // int abs(int a) { if (a<0) return -a; return a; }   (computed early return)
    check(
        "abs",
        "(I)I",
        vec![
            0x1a, 0x9c, 0x00, 0x06, // iload_0; ifge +6 → 7
            0x1a, 0x74, 0xac, // iload_0; ineg; ireturn
            0x1a, 0xac, // iload_0; ireturn
        ],
        1,
        1,
        &[(vec![-5], 5), (vec![0], 0), (vec![7], 7), (vec![-100], 100)],
    );
}

// ── int truncation conversions (i2b / i2c / i2s) ───────────────────────
//
// Step 3 (IR gate relaxation): the IR builder now lowers i2b/i2c/i2s (it used to
// bail → single-pass). These decompose to existing shift/and ops, so the
// optimizing IR path handles byte/char/short-truncating int methods. The
// harness confirms IR == single-pass for the sign/zero-extension edge cases.

#[test]
fn ir_vs_singlepass_i2b() {
    // int f(int a) { return (byte)a; }   iload_0; i2b; ireturn
    check(
        "i2b",
        "(I)I",
        vec![0x1a, 0x91, 0xac],
        1,
        1,
        &[
            (vec![127], 127),
            (vec![128], -128),
            (vec![256], 0),
            (vec![-1], -1),
            (vec![300], 44),
        ],
    );
}

#[test]
fn ir_vs_singlepass_i2c() {
    // int f(int a) { return (char)a; }   iload_0; i2c; ireturn
    check(
        "i2c",
        "(I)I",
        vec![0x1a, 0x92, 0xac],
        1,
        1,
        &[
            (vec![65], 65),
            (vec![-1], 65535),
            (vec![65536], 0),
            (vec![-65536], 0),
            (vec![0xABCD], 0xABCD),
        ],
    );
}

#[test]
fn ir_vs_singlepass_i2s() {
    // int f(int a) { return (short)a; }   iload_0; i2s; ireturn
    check(
        "i2s",
        "(I)I",
        vec![0x1a, 0x93, 0xac],
        1,
        1,
        &[
            (vec![32767], 32767),
            (vec![32768], -32768),
            (vec![65536], 0),
            (vec![-1], -1),
        ],
    );
}

#[test]
fn ir_vs_singlepass_i2b_chained() {
    // int g(int a) { return (byte)a + 1000; }   — the truncated value must feed
    // a following int op correctly (sipush 1000; iadd).
    check(
        "i2b_chained",
        "(I)I",
        vec![0x1a, 0x91, 0x11, 0x03, 0xe8, 0x60, 0xac],
        1,
        1,
        &[(vec![128], 872), (vec![127], 1127), (vec![-1], 999)],
    );
}

// ── switches (tableswitch / lookupswitch) ──────────────────────────────
//
// Step 3 second slice: the IR builder now lowers tableswitch/lookupswitch as a
// CMP-equality chain (the same shape single-pass emits), so int switch methods
// take the optimizing IR path. The harness confirms IR == single-pass for hits
// and the default.

#[test]
fn ir_vs_singlepass_tableswitch() {
    // int f(int x) { switch (x) { case 0: return 10; case 1: return 20;
    //                             case 2: return 30; default: return 99; } }
    // tableswitch at pc 1 → padding to pc 4; table low=0 high=2.
    check(
        "tswitch",
        "(I)I",
        vec![
            0x1a, // 0: iload_0
            0xaa, // 1: tableswitch
            0x00, 0x00, // 2-3: padding
            0x00, 0x00, 0x00, 0x24, // 4-7: default = +36 → 37
            0x00, 0x00, 0x00, 0x00, // 8-11: low = 0
            0x00, 0x00, 0x00, 0x02, // 12-15: high = 2
            0x00, 0x00, 0x00, 0x1b, // 16-19: case 0 = +27 → 28
            0x00, 0x00, 0x00, 0x1e, // 20-23: case 1 = +30 → 31
            0x00, 0x00, 0x00, 0x21, // 24-27: case 2 = +33 → 34
            0x10, 0x0a, 0xac, // 28-30: bipush 10; ireturn
            0x10, 0x14, 0xac, // 31-33: bipush 20; ireturn
            0x10, 0x1e, 0xac, // 34-36: bipush 30; ireturn
            0x10, 0x63, 0xac, // 37-39: bipush 99; ireturn (default)
        ],
        1,
        1,
        &[
            (vec![0], 10),
            (vec![1], 20),
            (vec![2], 30),
            (vec![3], 99),
            (vec![-1], 99),
            (vec![100], 99),
        ],
    );
}

#[test]
fn ir_vs_singlepass_lookupswitch() {
    // int g(int x) { switch (x) { case 10: return 1; case 20: return 2;
    //                             default: return 0; } }
    check(
        "lswitch",
        "(I)I",
        vec![
            0x1a, // 0: iload_0
            0xab, // 1: lookupswitch
            0x00, 0x00, // 2-3: padding
            0x00, 0x00, 0x00, 0x1f, // 4-7: default = +31 → 32
            0x00, 0x00, 0x00, 0x02, // 8-11: npairs = 2
            0x00, 0x00, 0x00, 0x0a, // 12-15: match 10
            0x00, 0x00, 0x00, 0x1b, // 16-19: offset +27 → 28
            0x00, 0x00, 0x00, 0x14, // 20-23: match 20
            0x00, 0x00, 0x00, 0x1d, // 24-27: offset +29 → 30
            0x04, 0xac, // 28-29: iconst_1; ireturn
            0x05, 0xac, // 30-31: iconst_2; ireturn
            0x03, 0xac, // 32-33: iconst_0; ireturn (default)
        ],
        1,
        1,
        &[
            (vec![10], 1),
            (vec![20], 2),
            (vec![0], 0),
            (vec![15], 0),
            (vec![-5], 0),
        ],
    );
}

#[test]
fn ir_vs_singlepass_sum_loop() {
    // int sum(int n) { int s=0; for (int i=0;i<n;i++) s+=i; return s; }
    //   locals: 0=n(param), 1=s, 2=i
    check(
        "sum",
        "(I)I",
        vec![
            0x03, 0x3c, // iconst_0; istore_1   (s)
            0x03, 0x3d, // iconst_0; istore_2   (i)
            0x1c, 0x1a, 0xa2, 0x00, 0x0d, // iload_2; iload_0; if_icmpge +13 → 19
            0x1b, 0x1c, 0x60, 0x3c, // iload_1; iload_2; iadd; istore_1
            0x84, 0x02, 0x01, // iinc 2, 1
            0xa7, 0xff, 0xf4, // goto -12 → 4
            0x1b, 0xac, // iload_1; ireturn
        ],
        3,
        1,
        &[
            (vec![0], 0),
            (vec![1], 0),
            (vec![5], 10),
            (vec![10], 45),
            (vec![100], 4950),
        ],
    );
}

// ── instance-field reads (getfield → Op::Load) ─────────────────────────
//
// Step 3 (field/call frontier, slice 1): the IR builder now lowers an
// int-category `getfield` into `Op::Load`, so a method whose only heap op is a
// field read takes the optimizing IR path. Unlike the pure-arithmetic corpus
// above, these execute a real memory access, so the harness builds a synthetic
// heap object laid out exactly as the VM lays one out — header + 16-byte
// `Value` cells — and passes its address as the receiver. Both backends read
// the same bytes, so IR == single-pass == host proves the inline-getfield ABI
// (null → 0, else MOVSXD the 32-bit `Value::Int` payload) is byte-faithful.
//
// Only int-category fields are exercised (the only shape the builder lowers);
// every `putfield` still bails to single-pass (no IR `Op::Store` yet).

/// Build a synthetic heap object with `fields.len()` int fields, each holding
/// the given value. Mirrors the VM object layout: a `HEADER_SIZE`-byte header
/// followed by one `SLOT_SIZE`-byte `Value` cell per field; an int value lives
/// at `FIELD_CELL_PAYLOAD32_OFFSET` within its cell with a zero (`Value::Int`)
/// discriminant tag (the all-zero buffer supplies the tag). The returned
/// buffer must outlive every call that reads it.
fn make_object(fields: &[i32]) -> Vec<u64> {
    // u64 backing so the buffer is 8-aligned: the guarded inline getfield's
    // receiver check (alignment bit-test + region bounds) requires real
    // object headers to be 8-aligned, and Vec<u8> guarantees nothing.
    let bytes = HEADER_SIZE + fields.len() * SLOT_SIZE;
    let mut buf = vec![0u64; bytes.div_ceil(8)];
    let base = buf.as_mut_ptr() as *mut u8;
    for (i, &v) in fields.iter().enumerate() {
        let off = HEADER_SIZE + i * SLOT_SIZE + FIELD_CELL_PAYLOAD32_OFFSET;
        // SAFETY: off + 4 <= bytes <= buf.len()*8 by construction.
        unsafe { std::ptr::write_unaligned(base.add(off) as *mut i32, v) };
    }
    buf
}

/// Compile via the per-call `optimize` toggle, supplying a constant-pool field
/// resolver (`cp_idx → (field_index, type_tag)`) both backends need to resolve
/// the field layout.
fn compile_opt_fields(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    field_resolver: &dyn Fn(u16) -> Option<(usize, u8)>,
    optimize: bool,
) -> Option<CompiledMethod> {
    // No registered CompactLayout for this test's synthetic buffers -- None
    // (not a fabricated (0, false)) is the correct "no compact slot" value;
    // see cp_field_resolver's Option<(u32, bool)> contract in jit/src/lib.rs.
    let field_resolver_with_compact =
        |cp_idx| field_resolver(cp_idx).map(|(field_index, tag)| (field_index, tag, None));
    try_compile(
        cm,
        None,
        Some(&field_resolver_with_compact),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        helpers,
        None,
        None,
        None,
        None,
        optimize,
        false,
        false,
        false,
        false,
        false, // ir_emit_fp
        None,
    )
}

/// Compile a single-object-parameter `(L…;)I` method both ways, then for each
/// `(field_values, expected)` case build an object from `field_values`, call
/// both bodies with its address, and assert IR == single-pass == `expected`.
fn check_field(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    max_locals: u16,
    field_resolver: &dyn Fn(u16) -> Option<(usize, u8)>,
    cases: &[(Vec<i32>, i32)],
) {
    let helpers = dummy_helpers();
    let cm = cached(name, descriptor, code, max_locals, 1);
    let ir = compile_opt_fields(&cm, &helpers, field_resolver, true)
        .unwrap_or_else(|| panic!("{name}: optimize=true (IR pipeline) failed to compile"));
    let sp = compile_opt_fields(&cm, &helpers, field_resolver, false)
        .unwrap_or_else(|| panic!("{name}: optimize=false (single-pass) failed to compile"));
    // getfield methods are `needs_heap` (the checked-helper fallback wants the
    // hidden vm_ptr), so the artifact ABI has the context in ARG0 and the
    // receiver in ARG1 — calling `try_call` without a context would feed the
    // receiver INTO the context slot and a garbage register into local 0. The
    // int getfield's guarded-inline fast path never dereferences the context,
    // so a zeroed dummy buffer is a safe placeholder (a receiver that fails
    // the inline guard reaches the panicking stub helper, which is exactly
    // the signal these tests want).
    let dummy_vm = [0u8; 64];
    for (fields, expected) in cases {
        let obj = make_object(fields);
        let args = [obj.as_ptr() as i64];
        // SAFETY: both bodies were JIT-compiled from valid getfield bytecode;
        // `obj` is a live, 8-aligned, correctly-laid-out object whose address
        // is the sole (reference) Java argument, and it outlives both calls
        // (dropped at the end of this iteration). The guarded inline getfield
        // raw-loads it (inside the wide-open TEST_REGION_BOUNDS); no runtime
        // helper is reachable for a valid receiver.
        let call = |m: &CompiledMethod| unsafe {
            if m.needs_context() {
                m.try_call_with_context(dummy_vm.as_ptr() as i64, &args)
            } else {
                m.try_call(&args)
            }
        };
        let r_sp =
            call(&sp).unwrap_or_else(|e| panic!("{name}: single-pass call {fields:?}: {e:?}"));
        let r_ir = call(&ir).unwrap_or_else(|e| panic!("{name}: IR call {fields:?}: {e:?}"));
        assert_eq!(
            r_ir as i32, r_sp as i32,
            "{name}: IR vs single-pass DIVERGE for {fields:?}: IR={}, single-pass={}",
            r_ir as i32, r_sp as i32,
        );
        assert_eq!(
            r_ir as i32, *expected,
            "{name}: both backends agree but disagree with host for {fields:?}: got {}, expected {expected}",
            r_ir as i32,
        );
        drop(obj); // keep the object alive until after both calls
    }
}

#[test]
fn ir_vs_singlepass_getfield_simple() {
    // static int get(Corpus o) { return o.x; }   (field 0 = int)
    //   aload_0; getfield #2; ireturn
    let resolver = |cp: u16| if cp == 2 { Some((0, b'I')) } else { None };
    check_field(
        "getfield_simple",
        "(Lpkg/Corpus;)I",
        vec![0x2a, 0xb4, 0x00, 0x02, 0xac],
        1,
        &resolver,
        &[
            (vec![5], 5),
            (vec![-7], -7),
            (vec![0], 0),
            (vec![i32::MIN], i32::MIN),
            (vec![i32::MAX], i32::MAX),
        ],
    );
}

#[test]
fn ir_vs_singlepass_getfield_via_astore_local() {
    // static int get(Corpus o) { Corpus p = o; return p.x; }
    // Round-trips the receiver ref through a LOCAL via `astore`/`aload` — the
    // shape real javac emits for an object local. The IR builder had no `astore`
    // handler, so any method storing a ref to a local bailed to single-pass;
    // this proves the now-lowered `astore`/`aload` round-trip executes
    // identically (IR == single-pass == host) on the int-field path.
    //   aload_0; astore_1; aload_1; getfield #2; ireturn
    //   2a 4c 2b b4 00 02 ac
    let resolver = |cp: u16| if cp == 2 { Some((0, b'I')) } else { None };
    check_field(
        "getfield_via_astore",
        "(Lpkg/Corpus;)I",
        vec![0x2a, 0x4c, 0x2b, 0xb4, 0x00, 0x02, 0xac],
        2, // local 0 = receiver param, local 1 = p
        &resolver,
        &[
            (vec![5], 5),
            (vec![-7], -7),
            (vec![0], 0),
            (vec![i32::MIN], i32::MIN),
            (vec![i32::MAX], i32::MAX),
        ],
    );
}

#[test]
fn ir_vs_singlepass_getfield_two_fields_sum() {
    // static int sum(Corpus o) { return o.x + o.y; }   (fields 0,1 = int)
    //   aload_0; getfield #2; aload_0; getfield #3; iadd; ireturn
    let resolver = |cp: u16| match cp {
        2 => Some((0, b'I')),
        3 => Some((1, b'I')),
        _ => None,
    };
    check_field(
        "getfield_sum",
        "(Lpkg/Corpus;)I",
        vec![
            0x2a, 0xb4, 0x00, 0x02, // aload_0; getfield #2 (x)
            0x2a, 0xb4, 0x00, 0x03, // aload_0; getfield #3 (y)
            0x60, 0xac, // iadd; ireturn
        ],
        1,
        &resolver,
        &[
            (vec![3, 4], 7),
            (vec![10, -3], 7),
            (vec![-5, -6], -11),
            (vec![i32::MAX, 1], i32::MIN), // wraps
        ],
    );
}

#[test]
fn ir_vs_singlepass_getfield_branch() {
    // static int sign(Corpus o) { int v = o.x; if (v < 0) return -1; return 1; }
    // A field read feeding a conditional branch + two return points — proves the
    // Load is pinned to the right control path and the value flows into a φ-free
    // multi-return shape.
    //   0: aload_0           2a
    //   1: getfield #2       b4 00 02
    //   4: istore_1          3c
    //   5: iload_1           1b
    //   6: iflt +5 → 11      9b 00 05
    //   9: iconst_1; ireturn 04 ac
    //  11: iconst_m1;ireturn 02 ac
    let resolver = |cp: u16| if cp == 2 { Some((0, b'I')) } else { None };
    check_field(
        "getfield_branch",
        "(Lpkg/Corpus;)I",
        vec![
            0x2a, 0xb4, 0x00, 0x02, 0x3c, 0x1b, 0x9b, 0x00, 0x05, 0x04, 0xac, 0x02, 0xac,
        ],
        2,
        &resolver,
        &[
            (vec![5], 1),
            (vec![-5], -1),
            (vec![0], 1),
            (vec![i32::MIN], -1),
            (vec![i32::MAX], 1),
        ],
    );
}

#[test]
fn ir_vs_singlepass_getfield_loop() {
    // static int f(Corpus o) { int s=0; for (int i=0;i<o.n;i++) s += o.x; return s; }
    // getfield in BOTH the loop condition (o.n) and body (o.x), re-read each
    // iteration (LICM is default-off), under a loop with carried phis + a memory
    // phi — proves Op::Load schedules correctly inside a loop body.
    //   locals: 0=o, 1=s, 2=i ; fields: 0=x, 1=n
    let resolver = |cp: u16| match cp {
        2 => Some((0, b'I')), // x
        3 => Some((1, b'I')), // n
        _ => None,
    };
    check_field(
        "getfield_loop",
        "(Lpkg/Corpus;)I",
        vec![
            0x03, 0x3c, // 0: iconst_0; istore_1   (s = 0)
            0x03, 0x3d, // 2: iconst_0; istore_2   (i = 0)
            0x1c, 0x2a, 0xb4, 0x00, 0x03, // 4: iload_2; aload_0; getfield #3 (n)
            0xa2, 0x00, 0x10, // 9: if_icmpge +16 → 25
            0x1b, 0x2a, 0xb4, 0x00, 0x02, // 12: iload_1; aload_0; getfield #2 (x)
            0x60, 0x3c, // 17: iadd; istore_1
            0x84, 0x02, 0x01, // 19: iinc 2, 1
            0xa7, 0xff, 0xee, // 22: goto -18 → 4
            0x1b, 0xac, // 25: iload_1; ireturn
        ],
        3,
        &resolver,
        &[
            (vec![5, 3], 15),
            (vec![5, 0], 0),
            (vec![7, 4], 28),
            (vec![-2, 3], -6),
            (vec![3, 10], 30),
        ],
    );
}

// ── instance-field writes (putfield → Op::Store) ───────────────────────
//
// Step 3 slice 2: the IR builder lowers an int-category `putfield` to
// `Op::Store`, the first store the production IR path emits. Soundness rests on
// the memory token chain: every memory op consumes the prior token and produces
// a new one, so the scheduler's input-edge topological sort serialises them
// (RAW/WAR/WAW all preserved). The lowerer inlines the `jit_putfield_int` heap
// write; the harness gives single-pass (which lowers to `CALL jit_putfield_int`)
// a faithful stub, so both backends produce identical cells. Each case runs each
// backend against its OWN fresh object and compares the return value AND the
// post-call object state (so a dropped store is caught).

/// `dummy_helpers` with a real `putfield_int` that writes a `Value::Int` cell
/// into the synthetic object — single-pass lowers an int `putfield` to a
/// `CALL jit_putfield_int`, so it needs a live helper. Matches the inline IR
/// store byte-for-byte (discriminant 0 + 32-bit payload, high qword cleared).
fn field_helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn putfield_int(obj: i64, field_index: i64, val: i64) {
        if obj == 0 {
            return;
        }
        let p = obj as *mut u8;
        let off = HEADER_SIZE + field_index as usize * SLOT_SIZE;
        // The synthetic object is a byte-aligned `Vec<u8>`, so use unaligned
        // writes (the JIT's own `MOV` stores are unaligned-safe on x86; a real
        // heap object is aligned, where `jit_putfield_int`'s aligned write is
        // fine). The resulting cell bytes are identical either way.
        std::ptr::write_unaligned(p.add(off) as *mut u32, 0); // Value::Int discriminant
        std::ptr::write_unaligned(
            p.add(off + FIELD_CELL_PAYLOAD32_OFFSET) as *mut i32,
            val as i32,
        );
        std::ptr::write_unaligned(p.add(off + 8) as *mut u64, 0); // high qword
    }
    let mut h = dummy_helpers();
    h.putfield_int = putfield_int as *const () as usize;
    h
}

/// Read the int payload of field `i` from a synthetic object buffer.
fn read_field(buf: &[u64], i: usize) -> i32 {
    let off = HEADER_SIZE + i * SLOT_SIZE + FIELD_CELL_PAYLOAD32_OFFSET;
    let base = buf.as_ptr() as *const u8;
    // SAFETY: off + 4 is within the buffer by make_object's construction.
    unsafe { std::ptr::read_unaligned(base.add(off) as *const i32) }
}

/// Compile a `(L…; <int args>)I` method both ways; for each case run each
/// backend against its OWN fresh object built from `init`, with `extra` int
/// args after the receiver, then assert the return value AND the post-call
/// field state agree across backends and with the host expectation.
fn check_field_rw(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
    field_resolver: &dyn Fn(u16) -> Option<(usize, u8)>,
    n_fields: usize,
    cases: &[(Vec<i32>, Vec<i64>, i32, Vec<i32>)],
) {
    let helpers = field_helpers();
    let cm = cached(name, descriptor, code, max_locals, num_params);
    let ir = compile_opt_fields(&cm, &helpers, field_resolver, true)
        .unwrap_or_else(|| panic!("{name}: optimize=true (IR pipeline) failed to compile"));
    let sp = compile_opt_fields(&cm, &helpers, field_resolver, false)
        .unwrap_or_else(|| panic!("{name}: optimize=false (single-pass) failed to compile"));
    for (init, extra, expected, after) in cases {
        // A `putfield` method is `needs_heap` in the single-pass backend, so its
        // body takes a hidden VM-context pointer as the first argument
        // (`try_call_with_context`) — the receiver and value follow. The inline
        // IR store needs no context (`needs_context() == false`) and is called
        // via `try_call`. An int putfield never dereferences the context, so a
        // zeroed dummy buffer is a safe placeholder. Each backend mutates its
        // own fresh object.
        let dummy_vm = [0u8; 64];
        let run = |m: &CompiledMethod| -> (i32, Vec<i32>) {
            let mut obj = make_object(init);
            let mut args = vec![obj.as_mut_ptr() as i64];
            args.extend_from_slice(extra);
            // SAFETY: `m` is JIT-compiled from valid field bytecode; `obj` is a
            // live, exclusively-owned, correctly-laid-out object whose address
            // is the receiver arg; the int `extra` args follow. The only helper
            // reachable is the live `putfield_int` stub. The context pointer (if
            // taken) is a live 64-byte buffer the int path never reads.
            let r = unsafe {
                if m.needs_context() {
                    m.try_call_with_context(dummy_vm.as_ptr() as i64, &args)
                } else {
                    m.try_call(&args)
                }
            }
            .unwrap_or_else(|e| panic!("{name}: call {init:?},{extra:?}: {e:?}"));
            let fields: Vec<i32> = (0..n_fields).map(|i| read_field(&obj, i)).collect();
            (r as i32, fields)
        };
        let (r_sp, after_sp) = run(&sp);
        let (r_ir, after_ir) = run(&ir);
        assert_eq!(
            r_ir, r_sp,
            "{name}: return IR vs single-pass DIVERGE for {init:?},{extra:?}: IR={r_ir}, sp={r_sp}",
        );
        assert_eq!(
            after_ir, after_sp,
            "{name}: object state IR vs single-pass DIVERGE for {init:?},{extra:?}: IR={after_ir:?}, sp={after_sp:?}",
        );
        assert_eq!(
            r_ir, *expected,
            "{name}: both backends agree but return disagrees with host for {init:?},{extra:?}: got {r_ir}, expected {expected}",
        );
        assert_eq!(
            &after_ir, after,
            "{name}: both backends agree but object state disagrees with host for {init:?},{extra:?}: got {after_ir:?}, expected {after:?}",
        );
    }
}

#[test]
fn ir_vs_singlepass_putfield_then_getfield() {
    // static int setget(Corpus o, int v) { o.x = v; return o.x; }
    // Store-then-load (RAW): the load MUST observe the store. A mis-ordered
    // load returns the initial field value instead.
    //   aload_0; iload_1; putfield #2; aload_0; getfield #2; ireturn
    let resolver = |cp: u16| if cp == 2 { Some((0, b'I')) } else { None };
    check_field_rw(
        "setget",
        "(Lpkg/Corpus;I)I",
        vec![
            0x2a, 0x1b, 0xb5, 0x00, 0x02, // aload_0; iload_1; putfield #2
            0x2a, 0xb4, 0x00, 0x02, 0xac, // aload_0; getfield #2; ireturn
        ],
        2,
        2,
        &resolver,
        1,
        &[
            (vec![99], vec![5], 5, vec![5]),
            (vec![0], vec![-7], -7, vec![-7]),
            (vec![3], vec![0], 0, vec![0]),
            (vec![7], vec![i32::MIN as i64], i32::MIN, vec![i32::MIN]),
        ],
    );
}

#[test]
fn ir_vs_singlepass_getfield_then_putfield() {
    // static int getset(Corpus o, int v) { int t = o.x; o.x = v; return t; }
    // Load-then-store (WAR): the returned old value MUST be read BEFORE the
    // store overwrites the field. A mis-ordered store returns the new value.
    //   aload_0; getfield #2; istore_2; aload_0; iload_1; putfield #2; iload_2; ireturn
    let resolver = |cp: u16| if cp == 2 { Some((0, b'I')) } else { None };
    check_field_rw(
        "getset",
        "(Lpkg/Corpus;I)I",
        vec![
            0x2a, 0xb4, 0x00, 0x02, 0x3d, // aload_0; getfield #2; istore_2
            0x2a, 0x1b, 0xb5, 0x00, 0x02, // aload_0; iload_1; putfield #2
            0x1c, 0xac, // iload_2; ireturn
        ],
        3,
        2,
        &resolver,
        1,
        &[
            (vec![99], vec![5], 99, vec![5]),
            (vec![42], vec![-1], 42, vec![-1]),
            (vec![0], vec![7], 0, vec![7]),
        ],
    );
}

#[test]
fn ir_vs_singlepass_putfield_pure_write() {
    // static int set(Corpus o, int v) { o.x = v; return v; }
    // The store's memory result is UNUSED (the method returns v, not a read of
    // the field), so DCE must still keep it — the post-call field state proves
    // the write happened. A dropped store leaves the field at its initial value.
    //   aload_0; iload_1; putfield #2; iload_1; ireturn
    let resolver = |cp: u16| if cp == 2 { Some((0, b'I')) } else { None };
    check_field_rw(
        "set",
        "(Lpkg/Corpus;I)I",
        vec![0x2a, 0x1b, 0xb5, 0x00, 0x02, 0x1b, 0xac],
        2,
        2,
        &resolver,
        1,
        &[
            (vec![99], vec![5], 5, vec![5]),
            (vec![0], vec![-9], -9, vec![-9]),
            (vec![123], vec![123], 123, vec![123]),
        ],
    );
}

#[test]
fn ir_vs_singlepass_putfield_two_fields() {
    // static int set2(Corpus o, int a, int b) { o.x=a; o.y=b; return o.x+o.y; }
    // Two stores then two loads — WAW between the stores, RAW from each load.
    //   aload_0; iload_1; putfield #2; aload_0; iload_2; putfield #3;
    //   aload_0; getfield #2; aload_0; getfield #3; iadd; ireturn
    let resolver = |cp: u16| match cp {
        2 => Some((0, b'I')),
        3 => Some((1, b'I')),
        _ => None,
    };
    check_field_rw(
        "set2",
        "(Lpkg/Corpus;II)I",
        vec![
            0x2a, 0x1b, 0xb5, 0x00, 0x02, // o.x = a
            0x2a, 0x1c, 0xb5, 0x00, 0x03, // o.y = b
            0x2a, 0xb4, 0x00, 0x02, // o.x
            0x2a, 0xb4, 0x00, 0x03, // o.y
            0x60, 0xac, // iadd; ireturn
        ],
        3,
        3,
        &resolver,
        2,
        &[
            (vec![0, 0], vec![3, 4], 7, vec![3, 4]),
            (vec![1, 2], vec![10, 20], 30, vec![10, 20]),
            (vec![5, 5], vec![-3, 3], 0, vec![-3, 3]),
        ],
    );
}

// ── Gap B: invokestatic → Op::Call via the invoke_dispatch helper ──────
//
// The IR builder lowers an int-only `invokestatic` in an oop-free method to
// `Op::Call`, which the lowerer dispatches as
//   i64 helper(vm_ptr, info_ptr, args_ptr, num_args)
// (the same ABI as single-pass), marshalling the Java args contiguously into a
// frame staging region. These tests supply a REAL stub `invoke_dispatch` so the
// generated code actually runs — validating the marshalling (values, order,
// count), the `needs_context` prologue, and the `i64::MIN` exception sentinel —
// without the full VM (the handoff's "real dispatch stub for a direct static
// call" option).

/// Compile a method through the IR pipeline WITH `ir_emit_calls` on, supplying an
/// invoke resolver and helpers (whose `invoke_dispatch` the caller has wired to a
/// stub). Returns the compiled body (needs_context, since it contains a call).
fn compile_with_dispatch(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    invoke_resolver: &dyn Fn(u16) -> Option<(String, String, String)>,
) -> Option<CompiledMethod> {
    try_compile(
        cm,
        None,
        None,
        None,
        Some(invoke_resolver),
        None,
        None,
        None,
        None,
        None,
        helpers,
        None,
        None,
        None,
        None,
        true,  // optimize (C2 / IR pipeline)
        true,  // ir_emit_calls (Gap B, invokestatic)
        true,  // ir_emit_special_calls (inc 24, invokespecial — inert without 0xb7)
        true,  // ir_emit_long (inc 28 — long call args; inert for non-long callers)
        true,  // ir_emit_virtual_calls (inc 26, invokevirtual/interface)
        false, // ir_emit_fp (inc 30 — inert for these int/long callers)
        None,
    )
}

/// Like [`compile_with_dispatch`] but with BOTH the call gate AND the FP gate on
/// (inc 32) — for a caller that consumes a `double`-returning `Op::Call` result.
fn compile_with_dispatch_fp(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    invoke_resolver: &dyn Fn(u16) -> Option<(String, String, String)>,
) -> Option<CompiledMethod> {
    try_compile(
        cm,
        None,
        None,
        None,
        Some(invoke_resolver),
        None,
        None,
        None,
        None,
        None,
        helpers,
        None,
        None,
        None,
        None,
        true, // optimize
        true, // ir_emit_calls
        true, // ir_emit_special_calls
        true, // ir_emit_long
        true, // ir_emit_virtual_calls
        true, // ir_emit_fp (inc 32 — D call returns)
        None,
    )
}

/// Stub `invoke_dispatch` that reads `num_args` i64 args from `args_ptr` and
/// returns an order- AND count-sensitive function of them, so a marshalling bug
/// (wrong order, wrong base, wrong count) produces a divergent result:
///   result = Σ arg[i] * 100^(n-1-i)   +   n * 1_000_000
unsafe extern "C" fn positional_dispatch(
    _vm: i64,
    _info: i64,
    args_ptr: i64,
    num_args: i64,
) -> i64 {
    let p = args_ptr as *const i64;
    let n = num_args as usize;
    let mut acc: i64 = 0;
    for i in 0..n {
        acc = acc * 100 + (*p.add(i) as i32 as i64);
    }
    acc + (n as i64) * 1_000_000
}

fn host_positional(args: &[i64]) -> i64 {
    let mut acc: i64 = 0;
    for &a in args {
        acc = acc * 100 + (a as i32 as i64);
    }
    acc + (args.len() as i64) * 1_000_000
}

#[test]
fn ir_vs_singlepass_invokestatic_two_int_args() {
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = positional_dispatch as *const () as usize;
    // static int f(int a, int b) { return g(a, b); }
    //   iload_0; iload_1; invokestatic #2; ireturn
    let code = vec![0x1a, 0x1b, 0xb8, 0x00, 0x02, 0xac];
    let cm = cached("f", "(II)I", code, 2, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(II)I".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch(&cm, &helpers, &resolver)
        .expect("IR compile of int invokestatic method");
    assert!(
        ir.needs_context(),
        "an Op::Call method must report needs_context (VM ptr as hidden arg 0)"
    );
    let dummy_vm = [0u8; 64];
    for (a, b) in [
        (3i64, 4i64),
        (0, 0),
        (7, 9),
        (-1, 2),
        (100, 200),
        (i32::MAX as i64, 1),
    ] {
        // SAFETY: `ir` is a finalized IR-compiled body taking (vm_ptr, a, b); the
        // dummy VM buffer is never dereferenced by the stub dispatch helper.
        let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[a, b]) }
            .unwrap_or_else(|e| panic!("call ({a},{b}): {e:?}"));
        assert_eq!(
            r,
            host_positional(&[a, b]),
            "invokestatic dispatch result for ({a}, {b})"
        );
    }
}

#[test]
fn ir_vs_singlepass_invokestatic_three_args_and_arith() {
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = positional_dispatch as *const () as usize;
    // static int f(int a, int b, int c) { return g(a, b, c) + 1; }
    //   iload_0; iload_1; iload_2; invokestatic #2; iconst_1; iadd; ireturn
    let code = vec![0x1a, 0x1b, 0x1c, 0xb8, 0x00, 0x02, 0x04, 0x60, 0xac];
    let cm = cached("f", "(III)I", code, 3, 3);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(III)I".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch(&cm, &helpers, &resolver).expect("IR compile of 3-arg method");
    let dummy_vm = [0u8; 64];
    for (a, b, c) in [(1i64, 2i64, 3i64), (9, 8, 7), (0, 0, 0), (-1, -2, -3)] {
        let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[a, b, c]) }
            .unwrap_or_else(|e| panic!("call ({a},{b},{c}): {e:?}"));
        // host: g(a,b,c) result, then + 1 (the trailing iconst_1; iadd).
        let expected = (host_positional(&[a, b, c]) as i32).wrapping_add(1) as i64;
        assert_eq!(r, expected, "3-arg dispatch + arith for ({a},{b},{c})");
    }
}

#[test]
fn ir_vs_singlepass_invokestatic_exception_sentinel() {
    // A callee that throws makes `invoke_dispatch` return the `i64::MIN` sentinel;
    // the JIT method must detect it and bail (returning the sentinel unchanged so
    // the VM takes the pending exception) rather than use it as a result.
    unsafe extern "C" fn throwing_dispatch(_vm: i64, _info: i64, _args: i64, _n: i64) -> i64 {
        i64::MIN
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = throwing_dispatch as *const () as usize;
    let code = vec![0x1a, 0x1b, 0xb8, 0x00, 0x02, 0xac];
    let cm = cached("f", "(II)I", code, 2, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(II)I".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch(&cm, &helpers, &resolver).expect("compile");
    let dummy_vm = [0u8; 64];
    let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[1, 2]) }.expect("call");
    assert_eq!(
        r,
        i64::MIN,
        "a throwing callee (i64::MIN) must bail and return the sentinel"
    );
}

#[test]
fn ir_vs_singlepass_invokestatic_reference_arg() {
    // inc 22: a REFERENCE argument is marshalled as its raw pointer. The method
    // takes an object + an int and passes both to a static call; the stub reads
    // field 0 off the object pointer and returns `o.x + n`, proving the pointer
    // is marshalled intact (not truncated/swapped) and the int arg follows it.
    //   static int f(Corpus o, int n) { return g(o, n); }
    //   aload_0; iload_1; invokestatic #2; ireturn
    unsafe extern "C" fn ref_dispatch(_vm: i64, _info: i64, args_ptr: i64, num_args: i64) -> i64 {
        assert_eq!(num_args, 2, "ref_dispatch expects (object, int)");
        let p = args_ptr as *const i64;
        let obj = *p; // arg0 = the object pointer (a reference)
        let n = *p.add(1) as i32 as i64; // arg1 = int
        let off = HEADER_SIZE + FIELD_CELL_PAYLOAD32_OFFSET; // field 0 int payload
        let x = std::ptr::read_unaligned((obj as *const u8).add(off) as *const i32) as i64;
        x + n
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = ref_dispatch as *const () as usize;
    let code = vec![0x2a, 0x1b, 0xb8, 0x00, 0x02, 0xac];
    let cm = cached("f", "(Lpkg/Corpus;I)I", code, 2, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(Lpkg/Corpus;I)I".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch(&cm, &helpers, &resolver)
        .expect("IR compile of reference-arg invokestatic method");
    assert!(ir.needs_context());
    let dummy_vm = [0u8; 64];
    for (xval, n) in [(5i32, 7i64), (-3, 2), (0, 0), (i32::MAX, 1)] {
        let obj = make_object(&[xval]);
        let args = [obj.as_ptr() as i64, n];
        // SAFETY: `obj` is a live, correctly-laid-out synthetic object; its
        // address is arg0 (a reference), `n` is arg1; the stub only reads field 0.
        let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &args) }
            .unwrap_or_else(|e| panic!("call x={xval},n={n}: {e:?}"));
        assert_eq!(
            r,
            xval as i64 + n,
            "reference-arg marshalling for x={xval}, n={n}"
        );
        drop(obj);
    }
}

#[test]
fn ir_vs_singlepass_invokestatic_long_arg() {
    // inc 28: a LONG argument is marshalled as ONE i64 slot (full 64-bit). The
    // method passes a long + an int to a static call returning int; the stub
    // reads BOTH halves of the long arg0 (so a truncated marshalling would
    // diverge) plus the int arg1.
    //   static int f(long a, int n) { return g(a, n); }   // g:(JI)I
    //   lload_0; iload_2; invokestatic #2; ireturn
    unsafe extern "C" fn longarg_dispatch(
        _vm: i64,
        _info: i64,
        args_ptr: i64,
        num_args: i64,
    ) -> i64 {
        assert_eq!(num_args, 2, "longarg_dispatch expects (long, int)");
        let p = args_ptr as *const i64;
        let a = *p; // arg0 = the FULL 64-bit long (one slot)
        let n = *p.add(1) as i32 as i64; // arg1 = int
        let hi = (a >> 32) as i32 as i64; // high 32 bits — 0 if truncated
        let lo = a as i32 as i64; // low 32 bits
        hi.wrapping_add(lo).wrapping_add(n) as i32 as i64
    }
    fn host(a: i64, n: i64) -> i32 {
        let hi = (a >> 32) as i32;
        let lo = a as i32;
        hi.wrapping_add(lo).wrapping_add(n as i32)
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = longarg_dispatch as *const () as usize;
    let code = vec![0x1e, 0x1c, 0xb8, 0x00, 0x02, 0xac];
    let cm = cached("f", "(JI)I", code, 3, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(JI)I".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch(&cm, &helpers, &resolver)
        .expect("IR compile of long-arg invokestatic method");
    assert!(ir.needs_context());
    let dummy_vm = [0u8; 64];
    for (a, n) in [
        (3i64, 7i64),
        (0x1234_5678_9abc_def0u64 as i64, 11), // high bits non-zero
        (-1, 2),
        (i64::MIN, 1),
        (0, 0),
    ] {
        // SAFETY: finalized Op::Call body taking (vm, long a, int n); the dummy
        // VM buffer is never dereferenced by the stub.
        let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[a, n]) }
            .unwrap_or_else(|e| panic!("call a={a},n={n}: {e:?}"));
        assert_eq!(
            r as i32,
            host(a, n),
            "long-arg marshalling (full 64-bit) for a={a}, n={n}"
        );
    }
}

// ── J (long) call RETURNS — the i64::MIN-sentinel disambiguation (post-inc-29) ──
//
// `static_call_shape` now accepts a `J` return, so an `Op::Call` may produce a
// long result in RAX. The dispatch helper signals a callee exception/deopt by
// returning the `i64::MIN` sentinel — which is bit-identical to a legitimate
// `Long.MIN_VALUE` return. These tests pin the call-site disambiguation: on the
// `RAX == i64::MIN` branch the JIT consults the out-of-band `dispatch_threw`
// peek, bailing ONLY when a genuine exception/deopt is pending and otherwise
// keeping the real value. The methods do a trailing `+ 1` after the call so the
// "kept the value (then +1)" and "bailed (returned the sentinel unchanged)"
// outcomes are observably different.

#[test]
fn ir_vs_singlepass_invokestatic_long_return() {
    // static long f(long a, long b) { return g(a, b) + 1; }   // g:(JJ)J
    //   lload_0; lload_2; invokestatic #2; lconst_1; ladd; lreturn
    // Common path (RAX != i64::MIN): a variety of long results — including
    // negative and full-64-bit values — must round-trip through the `JNE .keep`
    // fast path untouched. `dispatch_threw` must NEVER be consulted here.
    unsafe extern "C" fn sum_dispatch(_vm: i64, _info: i64, args_ptr: i64, num_args: i64) -> i64 {
        assert_eq!(num_args, 2, "sum_dispatch expects (long, long)");
        let p = args_ptr as *const i64;
        (*p).wrapping_add(*p.add(1)) // full 64-bit long sum
    }
    unsafe extern "C" fn poison_threw() -> i64 {
        panic!("dispatch_threw must not run when the call result is not i64::MIN");
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = sum_dispatch as *const () as usize;
    helpers.dispatch_threw = poison_threw as *const () as usize;
    let code = vec![0x1e, 0x20, 0xb8, 0x00, 0x02, 0x0a, 0x61, 0xad];
    let cm = cached("f", "(JJ)J", code, 4, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(JJ)J".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch(&cm, &helpers, &resolver)
        .expect("IR compile of long-returning invokestatic method");
    assert!(ir.needs_context());
    let dummy_vm = [0u8; 64];
    for (a, b) in [
        (3i64, 4i64),
        (-5, 2),
        (0x0123_4567_89ab_cdefu64 as i64, 0x10),
        (i64::MAX, 0), // sum = MAX (not the sentinel)
        (-1, -1),      // sum = -2 (high bit set, not the sentinel)
    ] {
        // SAFETY: finalized Op::Call body taking (vm, long a, long b); the dummy
        // VM buffer is never dereferenced by the stub.
        let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[a, b]) }
            .unwrap_or_else(|e| panic!("call a={a},b={b}: {e:?}"));
        assert_eq!(
            r,
            a.wrapping_add(b).wrapping_add(1),
            "long-return common path for a={a}, b={b}"
        );
    }
}

#[test]
fn ir_vs_singlepass_invokestatic_long_return_min_value_legit() {
    // The collision case: a callee that LEGITIMATELY returns Long.MIN_VALUE
    // (== i64::MIN) must NOT be misread as the deopt/exception sentinel.
    // `dispatch_threw` reports "no signal pending" (0), so the caller keeps the
    // value and runs the trailing `+ 1` → MIN_VALUE + 1. Pre-fix this returned
    // i64::MIN (the caller silently bailed at the call, skipping the `+ 1`).
    //   static long f(long a, long b) { return g(a, b) + 1; }
    unsafe extern "C" fn min_value_dispatch(_vm: i64, _i: i64, _a: i64, _n: i64) -> i64 {
        i64::MIN // a real Long.MIN_VALUE result — no exception
    }
    unsafe extern "C" fn never_threw() -> i64 {
        0 // no exception/deopt pending
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = min_value_dispatch as *const () as usize;
    helpers.dispatch_threw = never_threw as *const () as usize;
    let code = vec![0x1e, 0x20, 0xb8, 0x00, 0x02, 0x0a, 0x61, 0xad];
    let cm = cached("f", "(JJ)J", code, 4, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(JJ)J".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch(&cm, &helpers, &resolver).expect("compile");
    let dummy_vm = [0u8; 64];
    let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[1, 2]) }.expect("call");
    assert_eq!(
        r,
        i64::MIN.wrapping_add(1),
        "a legitimate Long.MIN_VALUE return must be kept (then +1), not bailed as a deopt"
    );
}

#[test]
fn ir_vs_singlepass_invokestatic_long_return_min_value_exception() {
    // The genuine-sentinel case: the callee threw, so the dispatch helper
    // returns i64::MIN AND `dispatch_threw` reports a pending signal (1). The
    // caller must BAIL — propagate the sentinel unchanged, skipping the trailing
    // `+ 1` — so the VM's post-JIT path routes the pending exception.
    //   static long f(long a, long b) { return g(a, b) + 1; }
    unsafe extern "C" fn throwing_dispatch(_vm: i64, _i: i64, _a: i64, _n: i64) -> i64 {
        i64::MIN // the deopt/exception sentinel
    }
    unsafe extern "C" fn did_throw() -> i64 {
        1 // a real exception/deopt is pending
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = throwing_dispatch as *const () as usize;
    helpers.dispatch_threw = did_throw as *const () as usize;
    let code = vec![0x1e, 0x20, 0xb8, 0x00, 0x02, 0x0a, 0x61, 0xad];
    let cm = cached("f", "(JJ)J", code, 4, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(JJ)J".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch(&cm, &helpers, &resolver).expect("compile");
    let dummy_vm = [0u8; 64];
    let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[1, 2]) }.expect("call");
    assert_eq!(
        r,
        i64::MIN,
        "a throwing callee (i64::MIN + pending signal) must bail and propagate the sentinel \
         (the trailing +1 must NOT run)"
    );
}

#[test]
fn ir_vs_singlepass_invokespecial_instance_call() {
    // inc 24: a resolved non-`<init>` `invokespecial` (a private / `super.` /
    // otherwise non-virtual instance call) lowers to `Op::Call` with the
    // receiver marshalled as arg0 and `invoke_kind == 1`. Structurally this
    // mirrors the inc-22 reference-arg test, but the object is the IMPLICIT
    // receiver: the target descriptor is `(I)I`, not `(Lpkg/Corpus;I)I`, so the
    // IR builder must compute `num_jit_args = 1 (descriptor) + 1 (receiver)` and
    // place the receiver first. The stub reads field 0 off the receiver pointer
    // and returns `recv.x + n`, so a wrong receiver/arg order, base, or count
    // diverges.
    //   int f(Corpus o, int n) { return o.g(n); }   // g private → invokespecial
    //   aload_0; iload_1; invokespecial #2; ireturn
    unsafe extern "C" fn recv_dispatch(_vm: i64, _info: i64, args_ptr: i64, num_args: i64) -> i64 {
        assert_eq!(num_args, 2, "recv_dispatch expects (receiver, int)");
        let p = args_ptr as *const i64;
        let recv = *p; // arg0 = the receiver pointer (invokespecial `this`)
        let n = *p.add(1) as i32 as i64; // arg1 = int
        let off = HEADER_SIZE + FIELD_CELL_PAYLOAD32_OFFSET; // field 0 int payload
        let x = std::ptr::read_unaligned((recv as *const u8).add(off) as *const i32) as i64;
        x + n
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = recv_dispatch as *const () as usize;
    let code = vec![0x2a, 0x1b, 0xb7, 0x00, 0x02, 0xac];
    let cm = cached("f", "(Lpkg/Corpus;I)I", code, 2, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            // The invokespecial target: instance method `g(I)I` (receiver implicit).
            Some(("pkg/Corpus".into(), "g".into(), "(I)I".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch(&cm, &helpers, &resolver)
        .expect("IR compile of invokespecial instance method");
    assert!(
        ir.needs_context(),
        "an Op::Call method must report needs_context"
    );
    let dummy_vm = [0u8; 64];
    for (xval, n) in [(5i32, 7i64), (-3, 2), (0, 0), (i32::MAX, 1)] {
        let obj = make_object(&[xval]);
        let args = [obj.as_ptr() as i64, n];
        // SAFETY: `obj` is a live, correctly-laid-out synthetic object; its
        // address is the receiver (arg0), `n` is arg1; the stub only reads field 0.
        let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &args) }
            .unwrap_or_else(|e| panic!("call x={xval},n={n}: {e:?}"));
        assert_eq!(
            r,
            xval as i64 + n,
            "invokespecial receiver marshalling for x={xval}, n={n}"
        );
        drop(obj);
    }
}

#[test]
fn ir_vs_singlepass_invokevirtual_instance_call() {
    // inc 25: a resolved `invokevirtual` lowers to `Op::Call` with the receiver
    // marshalled as arg0 and `invoke_kind == 0`. Identical machine-level shape
    // to the inc-24 invokespecial test (receiver-first, `num_jit_args == 2`,
    // `needs_context`); only the opcode (0xb6) and dispatch kind differ. The
    // stub reads field 0 off the receiver and returns `recv.x + n`.
    //   int f(Corpus o, int n) { return o.g(n); }   // g virtual → invokevirtual
    //   aload_0; iload_1; invokevirtual #2; ireturn
    unsafe extern "C" fn recv_dispatch(_vm: i64, _info: i64, args_ptr: i64, num_args: i64) -> i64 {
        assert_eq!(num_args, 2, "recv_dispatch expects (receiver, int)");
        let p = args_ptr as *const i64;
        let recv = *p; // arg0 = the receiver pointer
        let n = *p.add(1) as i32 as i64; // arg1 = int
        let off = HEADER_SIZE + FIELD_CELL_PAYLOAD32_OFFSET; // field 0 int payload
        let x = std::ptr::read_unaligned((recv as *const u8).add(off) as *const i32) as i64;
        x + n
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = recv_dispatch as *const () as usize;
    let code = vec![0x2a, 0x1b, 0xb6, 0x00, 0x02, 0xac];
    let cm = cached("f", "(Lpkg/Corpus;I)I", code, 2, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Corpus".into(), "g".into(), "(I)I".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch(&cm, &helpers, &resolver)
        .expect("IR compile of invokevirtual instance method");
    assert!(
        ir.needs_context(),
        "an Op::Call method must report needs_context"
    );
    let dummy_vm = [0u8; 64];
    for (xval, n) in [(5i32, 7i64), (-3, 2), (0, 0), (i32::MAX, 1)] {
        let obj = make_object(&[xval]);
        let args = [obj.as_ptr() as i64, n];
        // SAFETY: `obj` is a live, correctly-laid-out synthetic object; its
        // address is the receiver (arg0), `n` is arg1; the stub only reads field 0.
        let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &args) }
            .unwrap_or_else(|e| panic!("call x={xval},n={n}: {e:?}"));
        assert_eq!(
            r,
            xval as i64 + n,
            "invokevirtual receiver marshalling for x={xval}, n={n}"
        );
        drop(obj);
    }
}

#[test]
fn ir_vs_singlepass_invokeinterface_instance_call() {
    // inc 25: a resolved `invokeinterface` lowers to `Op::Call` with the receiver
    // marshalled as arg0 and `invoke_kind == 2`. The decisive extra coverage vs.
    // the invokevirtual test is the FIVE-byte instruction encoding
    // (0xb9, cp_hi, cp_lo, count, 0): the builder and both length walkers must
    // advance pc += 5, or the trailing `ireturn` is mis-located and the method
    // either bails or miscompiles. The stub reads field 0 off the receiver.
    //   int f(Iface o, int n) { return o.g(n); }   // g interface → invokeinterface
    //   aload_0; iload_1; invokeinterface #2, 2, 0; ireturn
    unsafe extern "C" fn recv_dispatch(_vm: i64, _info: i64, args_ptr: i64, num_args: i64) -> i64 {
        assert_eq!(num_args, 2, "recv_dispatch expects (receiver, int)");
        let p = args_ptr as *const i64;
        let recv = *p;
        let n = *p.add(1) as i32 as i64;
        let off = HEADER_SIZE + FIELD_CELL_PAYLOAD32_OFFSET;
        let x = std::ptr::read_unaligned((recv as *const u8).add(off) as *const i32) as i64;
        x + n
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = recv_dispatch as *const () as usize;
    // invokeinterface is 5 bytes: opcode, cp_hi, cp_lo, count(=2: receiver+int), 0.
    let code = vec![0x2a, 0x1b, 0xb9, 0x00, 0x02, 0x02, 0x00, 0xac];
    let cm = cached("f", "(Lpkg/Iface;I)I", code, 2, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Iface".into(), "g".into(), "(I)I".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch(&cm, &helpers, &resolver)
        .expect("IR compile of invokeinterface instance method");
    assert!(
        ir.needs_context(),
        "an Op::Call method must report needs_context"
    );
    let dummy_vm = [0u8; 64];
    for (xval, n) in [(5i32, 7i64), (-3, 2), (0, 0), (i32::MAX, 1)] {
        let obj = make_object(&[xval]);
        let args = [obj.as_ptr() as i64, n];
        // SAFETY: as above — receiver is arg0, `n` is arg1; the stub reads field 0.
        let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &args) }
            .unwrap_or_else(|e| panic!("call x={xval},n={n}: {e:?}"));
        assert_eq!(
            r,
            xval as i64 + n,
            "invokeinterface (5-byte) receiver marshalling for x={xval}, n={n}"
        );
        drop(obj);
    }
}

// ── inc 30: double/float XMM value tier ──────────────────────────────────
//
// Each corpus method takes `int` args and returns an `int`, computing with
// `float`/`double` internally — so the GPR i64-arg / i64-ret `try_call` ABI is
// exact and the FP work stays in XMM (FP params/returns are a follow-on). The
// host anchor is computed with Rust's float-to-int `as` cast, which saturates
// (NaN→0, ±overflow→MIN/MAX, truncate toward zero) — exactly the JVM `f2i`/`d2i`
// semantics the JIT must reproduce, so it catches a bug shared by both backends.

#[test]
fn ir_vs_singlepass_fadd_round_trip() {
    // int f(int a, int b) { return (int)((float)a + (float)b); }
    //   iload_0; i2f; iload_1; i2f; fadd; f2i; ireturn
    check_fp(
        "fadd",
        "(II)I",
        vec![0x1a, 0x86, 0x1b, 0x86, 0x62, 0x8b, 0xac],
        2,
        2,
        &[
            (vec![3, 4], 7),
            (vec![-5, 10], 5),
            // both < 2^24, so the float sum is exact
            (vec![1_000_000, 2_000_000], 3_000_000),
        ],
    );
}

#[test]
fn ir_vs_singlepass_fsub_fmul() {
    // int f(int a, int b) { return (int)((float)a - (float)b); }
    check_fp(
        "fsub",
        "(II)I",
        vec![0x1a, 0x86, 0x1b, 0x86, 0x66, 0x8b, 0xac],
        2,
        2,
        &[(vec![10, 3], 7), (vec![3, 10], -7), (vec![-4, -9], 5)],
    );
    // int f(int a, int b) { return (int)((float)a * (float)b); }
    check_fp(
        "fmul",
        "(II)I",
        vec![0x1a, 0x86, 0x1b, 0x86, 0x6a, 0x8b, 0xac],
        2,
        2,
        &[(vec![3, 4], 12), (vec![-5, 6], -30), (vec![7, 0], 0)],
    );
}

#[test]
fn ir_vs_singlepass_fdiv_with_inf_overflow() {
    // int f(int a, int b) { return (int)((float)a / (float)b); }
    //   iload_0; i2f; iload_1; i2f; fdiv; f2i; ireturn
    check_fp(
        "fdiv",
        "(II)I",
        vec![0x1a, 0x86, 0x1b, 0x86, 0x6e, 0x8b, 0xac],
        2,
        2,
        &[
            (vec![7, 2], 3),         // 3.5 → 3 (toward zero)
            (vec![-7, 2], -3),       // -3.5 → -3
            (vec![10, 3], 3),        // 3.333…
            (vec![5, 0], i32::MAX),  // +inf → INT_MAX (NaN/overflow fixup)
            (vec![-5, 0], i32::MIN), // -inf → INT_MIN
        ],
    );
}

#[test]
fn ir_vs_singlepass_dmul_with_overflow() {
    // int f(int a, int b) { return (int)((double)a * (double)b); }
    //   iload_0; i2d; iload_1; i2d; dmul; d2i; ireturn
    check_fp(
        "dmul",
        "(II)I",
        vec![0x1a, 0x87, 0x1b, 0x87, 0x6b, 0x8e, 0xac],
        2,
        2,
        &[
            (vec![3, 4], 12),
            (vec![-7, 6], -42),
            (vec![100_000, 100_000], i32::MAX),  // 1e10 → INT_MAX
            (vec![-100_000, 100_000], i32::MIN), // -1e10 → INT_MIN
        ],
    );
    // int f(int a, int b) { return (int)((double)a - (double)b); }  — dsub
    check_fp(
        "dsub",
        "(II)I",
        vec![0x1a, 0x87, 0x1b, 0x87, 0x67, 0x8e, 0xac],
        2,
        2,
        &[(vec![10, 3], 7), (vec![3, 10], -7)],
    );
}

#[test]
fn ir_vs_singlepass_fp_negation() {
    // int f(int a) { return (int)(-((float)a)); }  — fneg (sign-bit flip)
    check_fp(
        "fneg",
        "(I)I",
        vec![0x1a, 0x86, 0x76, 0x8b, 0xac],
        1,
        1,
        &[
            (vec![5], -5),
            (vec![-5], 5),
            (vec![0], 0),
            (vec![123456], -123456),
        ],
    );
    // int f(int a) { return (int)(-((double)a)); }  — dneg
    check_fp(
        "dneg",
        "(I)I",
        vec![0x1a, 0x87, 0x77, 0x8e, 0xac],
        1,
        1,
        &[(vec![5], -5), (vec![-5], 5), (vec![0], 0)],
    );
}

// ── FP remainder (frem/drem) — Slice A ──────────────────────────────────────
//
// JVMS FP remainder is `fmod`-style (truncated, sign of the dividend) with no
// single SSE instruction, so the IR `Op::Rem` Float/Double arm lowers to a
// `CALL` of the `jit_frem`/`jit_drem` runtime helper (operands in XMM0/XMM1,
// result XMM0). The SINGLE-PASS backend has no `frem`/`drem` arm — it bails such
// a method to the interpreter — so unlike the other FP ops there is no
// IR-vs-single-pass comparison; the contract is IR == host IEEE anchor. Rust's
// `%` on floats is itself `fmod`, the same reference the production helper
// delegates to, so it catches a miscompile in the call sequence / NaN handling.

/// Real `frem`/`drem` helper stubs for the FP-remainder tests — `extern "C"`
/// with the float ABI (args in XMM0/XMM1, return XMM0), exactly what the IR
/// `Op::Rem` site `CALL`s. Mirrors the production `jit_frem`/`jit_drem`.
unsafe extern "C" fn test_frem(a: f32, b: f32) -> f32 {
    a % b
}
unsafe extern "C" fn test_drem(a: f64, b: f64) -> f64 {
    a % b
}

/// [`dummy_helpers`] with the two FP-remainder helpers wired to real stubs (the
/// rest stay panic stubs — a `frem`/`drem` method calls no other helper).
fn frem_helpers() -> JitRuntimeHelpers {
    JitRuntimeHelpers { safepoint_flag_addr: 0, safepoint_slow_path: 0,
        jit_frem: test_frem as *const () as usize,
        jit_drem: test_drem as *const () as usize,
        self_call_stack_guard: 0,
        region_bounds_addr: TEST_REGION_BOUNDS.as_ptr() as usize,
        native_stack_floor_fn: 0,
        ..dummy_helpers()
    }
}

/// Compile an FP-remainder method through the IR pipeline ONLY and assert
/// IR == host (Rust float `%`). Single-pass bails `frem`/`drem`, so there is no
/// IR-vs-single-pass leg; the assert that single-pass returns `None` pins that
/// assumption (a future single-pass `frem` should switch this to `check_fp`).
fn check_frem(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
    cases: &[(Vec<i64>, i32)],
) {
    let helpers = frem_helpers();
    // Compile the IR body FIRST: a single-pass attempt bails (no frem arm) and
    // adds the method to the shared permanent bail-list (`mark_jit_bail_listed`),
    // which would then short-circuit this IR compile to `None`. So the
    // single-pass-bails check below runs on a DISTINCTLY-named twin method.
    let cm = cached(name, descriptor, code.clone(), max_locals, num_params);
    let ir = compile_fp_opt(&cm, &helpers, true)
        .unwrap_or_else(|| panic!("{name}: IR (FP) failed to compile frem/drem — gate/builder?"));
    // Pin the "single-pass bails frem/drem" assumption (a future single-pass
    // frem arm should switch this test to `check_fp`). Separate name so the
    // bail-list entry it creates can't shadow `cm` above.
    let twin = cached(
        &format!("{name}_spbail"),
        descriptor,
        code,
        max_locals,
        num_params,
    );
    assert!(
        compile_fp_opt(&twin, &helpers, false).is_none(),
        "{name}: single-pass unexpectedly compiled frem/drem — switch this test to check_fp",
    );
    for (args, expected) in cases {
        // SAFETY: IR-produced body from valid FP bytecode with an int signature;
        // the i64-arg/i64-ret ABI matches try_call and only frem/drem (wired
        // above) is reachable.
        let r_ir = unsafe { ir.try_call(args) }
            .unwrap_or_else(|e| panic!("{name}: IR call {args:?}: {e:?}"));
        assert_eq!(
            r_ir as i32, *expected,
            "{name}: IR vs host DIVERGE for {args:?}: IR={}, host={expected}",
            r_ir as i32,
        );
    }
}

#[test]
fn ir_fp_frem_integer_operands() {
    // int f(int a, int b) { return (int)((float)a % (float)b); }
    //   iload_0; i2f; iload_1; i2f; frem; f2i; ireturn
    check_frem(
        "frem",
        "(II)I",
        vec![0x1a, 0x86, 0x1b, 0x86, 0x72, 0x8b, 0xac],
        2,
        2,
        &[
            (vec![7, 3], 1),   // 7 % 3 = 1
            (vec![-7, 3], -1), // sign of dividend
            (vec![7, -3], 1),  // sign of dividend (not divisor)
            (vec![-7, -3], -1),
            (vec![8, 4], 0),
            (vec![10, 3], 1),
            (vec![5, 0], 0), // x % 0 = NaN; f2i(NaN) = 0
        ],
    );
}

#[test]
fn ir_fp_drem_integer_operands() {
    // int f(int a, int b) { return (int)((double)a % (double)b); }
    //   iload_0; i2d; iload_1; i2d; drem; d2i; ireturn
    check_frem(
        "drem",
        "(II)I",
        vec![0x1a, 0x87, 0x1b, 0x87, 0x73, 0x8e, 0xac],
        2,
        2,
        &[
            (vec![7, 3], 1),
            (vec![-7, 3], -1),
            (vec![7, -3], 1),
            (vec![-7, -3], -1),
            (vec![8, 4], 0),
            (vec![10, 3], 1),
            (vec![5, 0], 0), // NaN → 0
        ],
    );
}

#[test]
fn ir_fp_frem_fractional() {
    // int f(int a, int b) { return (int)(((a/4f) % (b/4f)) * 4f); } — genuinely
    // fractional intermediate remainders (0.25, …) prove the helper computes a
    // real fmod, not just integer-operand agreement. The *4 rescale lands on an
    // integer so f2i is lossless, and every value (n/4) is exact in binary FP.
    //   iload_0;i2f; iconst_4;i2f;fdiv; iload_1;i2f; iconst_4;i2f;fdiv; frem;
    //   iconst_4;i2f;fmul; f2i; ireturn
    check_frem(
        "frem_frac",
        "(II)I",
        vec![
            0x1a, 0x86, 0x07, 0x86, 0x6e, // (float)a / 4
            0x1b, 0x86, 0x07, 0x86, 0x6e, // (float)b / 4
            0x72, // frem
            0x07, 0x86, 0x6a, // * 4
            0x8b, 0xac, // f2i; ireturn
        ],
        2,
        2,
        &[
            (vec![7, 2], 1),   // 1.75 % 0.5 = 0.25 → *4 = 1
            (vec![9, 4], 1),   // 2.25 % 1.0 = 0.25 → 1
            (vec![-7, 2], -1), // -1.75 % 0.5 = -0.25 → -1
            (vec![10, 3], 1),  // 2.5 % 0.75 = 0.25 → 1
        ],
    );
}

#[test]
fn ir_fp_drem_fractional() {
    // Double analogue of `ir_fp_frem_fractional`.
    //   iload_0;i2d; iconst_4;i2d;ddiv; iload_1;i2d; iconst_4;i2d;ddiv; drem;
    //   iconst_4;i2d;dmul; d2i; ireturn
    check_frem(
        "drem_frac",
        "(II)I",
        vec![
            0x1a, 0x87, 0x07, 0x87, 0x6f, // (double)a / 4
            0x1b, 0x87, 0x07, 0x87, 0x6f, // (double)b / 4
            0x73, // drem
            0x07, 0x87, 0x6b, // * 4
            0x8e, 0xac, // d2i; ireturn
        ],
        2,
        2,
        &[
            (vec![7, 2], 1),
            (vec![9, 4], 1),
            (vec![-7, 2], -1),
            (vec![10, 3], 1),
        ],
    );
}

// ── FP method with int-div (Slice C: FP-slot deopt resume) ──────────────────
//
// Dropping the FP gate's `!method_has_int_div` exclusion admits an FP method
// containing an `idiv`/`irem` to the IR path. The int division carries a
// div-by-zero deopt guard; with FP-slot resume wired, an FP value live ACROSS
// that guard is reconstructed precisely on a deopt (the div-by-zero path is
// validated E2E vs HotSpot — caught ArithmeticException + correct FP value).
// Here, with a non-zero divisor (no deopt), the method must compile via IR and
// agree with single-pass (which also compiles idiv + FP) and the host anchor.

#[test]
fn ir_fp_method_with_int_div() {
    // int f(int a, int b) { float x = (float)a * 2; int q = a / b;
    //                       return (int)(x + (float)q); }
    // The FP value `x` is live on the operand stack ACROSS the idiv (the deopt
    // point), exercising an FP stack slot at the guard.
    //   iload_0; i2f; fconst_2; fmul;   // x = a*2.0f  [FP live]
    //   iload_0; iload_1; idiv;          // q = a/b     [idiv deopt guard]
    //   i2f; fadd; f2i; ireturn          // (int)(x + (float)q)
    check_fp(
        "fp_intdiv",
        "(II)I",
        vec![
            0x1a, 0x86, 0x0d, 0x6a, // (float)a * 2.0
            0x1a, 0x1b, 0x6c, // a / b
            0x86, 0x62, 0x8b, 0xac, // (float)q ; +x ; (int) ; return
        ],
        2,
        2,
        &[
            (vec![10, 2], 25),   // x=20.0, q=5  → 25
            (vec![7, 3], 16),    // x=14.0, q=2  → 16
            (vec![-8, 4], -18),  // x=-16.0, q=-2 → -18
            (vec![100, 7], 214), // x=200.0, q=14 → 214
        ],
    );
}

// ── FP array element access (faload/daload/fastore/dastore) — Slice B ───────
//
// The IR lowers an FP array access inline: array→RAX, index→RCX, the JVMS null
// + bounds deopt guards, then a `MOVSS`/`MOVSD` at `[RAX + RCX*elem_size +
// HEADER_SIZE]`. Single-pass compiles the same opcodes inline, so the
// non-faulting path is a true IR==single-pass==host differential (a synthetic
// array buffer is passed as the receiver, mirroring the getfield harness). The
// fault paths (null array / OOB index) deopt → the interpreter re-throws the
// exact NPE/AIOOBE; that parity is validated E2E vs HotSpot.

/// Build a synthetic `float[]` buffer: `length` at `ARRAY_LENGTH_OFFSET`, the
/// elements (4 bytes each) packed from `HEADER_SIZE`.
fn make_f32_array(elems: &[f32]) -> Vec<u8> {
    let mut buf = vec![0u8; HEADER_SIZE + elems.len() * 4];
    buf[ARRAY_LENGTH_OFFSET..ARRAY_LENGTH_OFFSET + 4]
        .copy_from_slice(&(elems.len() as u32).to_le_bytes());
    for (i, &v) in elems.iter().enumerate() {
        let off = HEADER_SIZE + i * 4;
        buf[off..off + 4].copy_from_slice(&v.to_bits().to_le_bytes());
    }
    buf
}

/// Build a synthetic `double[]` buffer (8 bytes per element).
fn make_f64_array(elems: &[f64]) -> Vec<u8> {
    let mut buf = vec![0u8; HEADER_SIZE + elems.len() * 8];
    buf[ARRAY_LENGTH_OFFSET..ARRAY_LENGTH_OFFSET + 4]
        .copy_from_slice(&(elems.len() as u32).to_le_bytes());
    for (i, &v) in elems.iter().enumerate() {
        let off = HEADER_SIZE + i * 8;
        buf[off..off + 8].copy_from_slice(&v.to_bits().to_le_bytes());
    }
    buf
}

#[test]
fn ir_fp_faload() {
    // static float fget(float[] a, int i) { return a[i]; }
    //   aload_0; iload_1; faload; freturn
    let helpers = dummy_helpers();
    let cm = cached("fget", "([FI)F", vec![0x2a, 0x1b, 0x30, 0xae], 2, 2);
    let ir = compile_fp_opt(&cm, &helpers, true).expect("fget IR (FP)");
    let sp = compile_fp_opt(&cm, &helpers, false).expect("fget single-pass");
    let elems = [1.5f32, -2.25, 0.0, 1234.5, f32::INFINITY];
    let arr = make_f32_array(&elems);
    for (i, &want) in elems.iter().enumerate() {
        let args = [arr.as_ptr() as i64, i as i64];
        // SAFETY: both bodies are JIT-compiled faload; `arr` is a live, correctly
        // laid-out float[] whose address is arg0, the index in-bounds.
        let r_sp = f32::from_bits(unsafe { sp.try_call(&args) }.unwrap() as u32);
        let r_ir = f32::from_bits(unsafe { ir.try_call(&args) }.unwrap() as u32);
        assert_eq!(r_ir.to_bits(), r_sp.to_bits(), "faload IR vs sp at {i}");
        assert_eq!(r_ir.to_bits(), want.to_bits(), "faload vs host at {i}");
    }
}

#[test]
fn ir_fp_daload() {
    // static double dget(double[] a, int i) { return a[i]; }
    //   aload_0; iload_1; daload; dreturn
    let helpers = dummy_helpers();
    let cm = cached("dget", "([DI)D", vec![0x2a, 0x1b, 0x31, 0xaf], 2, 2);
    let ir = compile_fp_opt(&cm, &helpers, true).expect("dget IR (FP)");
    let sp = compile_fp_opt(&cm, &helpers, false).expect("dget single-pass");
    let elems = [1.5f64, -2.25, 0.0, 1e300, f64::NEG_INFINITY];
    let arr = make_f64_array(&elems);
    for (i, &want) in elems.iter().enumerate() {
        let args = [arr.as_ptr() as i64, i as i64];
        let r_sp = f64::from_bits(unsafe { sp.try_call(&args) }.unwrap() as u64);
        let r_ir = f64::from_bits(unsafe { ir.try_call(&args) }.unwrap() as u64);
        assert_eq!(r_ir.to_bits(), r_sp.to_bits(), "daload IR vs sp at {i}");
        assert_eq!(r_ir.to_bits(), want.to_bits(), "daload vs host at {i}");
    }
}

#[test]
fn ir_fp_fastore() {
    // static void fset(float[] a, int i, float v) { a[i] = v; }
    //   aload_0; iload_1; fload_2; fastore; return
    let helpers = dummy_helpers();
    let cm = cached("fset", "([FIF)V", vec![0x2a, 0x1b, 0x24, 0x51, 0xb1], 3, 3);
    let ir = compile_fp_opt(&cm, &helpers, true).expect("fset IR (FP)");
    let sp = compile_fp_opt(&cm, &helpers, false).expect("fset single-pass");
    for (i, v) in [(0usize, 9.5f32), (2, -1.25), (4, 1e30)] {
        // Each backend writes its OWN fresh array; the element must agree.
        let read_back = |m: &CompiledMethod| -> u32 {
            let mut arr = make_f32_array(&[0.0; 5]);
            let args = [arr.as_mut_ptr() as i64, i as i64, v.to_bits() as i64];
            // SAFETY: JIT-compiled fastore; live float[] arg0, in-bounds index,
            // float value passed as bits (compact GPR FP ABI). No helper reached.
            unsafe { m.try_call(&args) }.unwrap();
            let off = HEADER_SIZE + i * 4;
            u32::from_le_bytes([arr[off], arr[off + 1], arr[off + 2], arr[off + 3]])
        };
        let got_sp = read_back(&sp);
        let got_ir = read_back(&ir);
        assert_eq!(got_ir, got_sp, "fastore IR vs sp at {i}");
        assert_eq!(got_ir, v.to_bits(), "fastore vs host at {i}");
    }
}

#[test]
fn ir_fp_dastore() {
    // static void dset(double[] a, int i, double v) { a[i] = v; }
    //   aload_0; iload_1; dload_2; dastore; return  (double v is cat-2 → slots 2-3;
    //   dload_2 == 0x28, NOT dload_0/0x26)
    let helpers = dummy_helpers();
    let cm = cached("dset", "([DID)V", vec![0x2a, 0x1b, 0x28, 0x52, 0xb1], 4, 3);
    let ir = compile_fp_opt(&cm, &helpers, true).expect("dset IR (FP)");
    let sp = compile_fp_opt(&cm, &helpers, false).expect("dset single-pass");
    for (i, v) in [(0usize, 9.5f64), (2, -1.25), (4, 1e300)] {
        let read_back = |m: &CompiledMethod| -> u64 {
            let mut arr = make_f64_array(&[0.0; 5]);
            let args = [arr.as_mut_ptr() as i64, i as i64, v.to_bits() as i64];
            unsafe { m.try_call(&args) }.unwrap();
            let off = HEADER_SIZE + i * 8;
            u64::from_le_bytes([
                arr[off],
                arr[off + 1],
                arr[off + 2],
                arr[off + 3],
                arr[off + 4],
                arr[off + 5],
                arr[off + 6],
                arr[off + 7],
            ])
        };
        let got_sp = read_back(&sp);
        let got_ir = read_back(&ir);
        assert_eq!(got_ir, got_sp, "dastore IR vs sp at {i}");
        assert_eq!(got_ir, v.to_bits(), "dastore vs host at {i}");
    }
}

// ── FP 3-way compares (fcmpl/fcmpg/dcmpl/dcmpg) — item 3 next slice ──────────
//
// The compare yields int {-1,0,1} feeding the existing `if<cond>`. The
// `ucomis`-based branchless lowering must agree with single-pass AND the host
// IEEE anchor for the ordered cases, AND honour the JVMS NaN-unordered rule
// (NaN → -1 for the `l` variants, +1 for the `g` variants), for a NaN in EITHER
// operand position.

#[test]
fn ir_vs_singlepass_fcmp_ordered() {
    // int f(int a, int b) { return Float.compare-ish: ((float)a) <cmp> ((float)b); }
    //   iload_0; i2f; iload_1; i2f; fcmp{l,g}; ireturn
    let ordered = &[
        (vec![1i64, 2], -1i32),
        (vec![2, 1], 1),
        (vec![5, 5], 0),
        (vec![-3, -3], 0),
        (vec![-5, 2], -1),
        (vec![2, -5], 1),
    ];
    // fcmpl (0x95) and fcmpg (0x96) are identical for ordered operands.
    check_fp(
        "fcmpl_ord",
        "(II)I",
        vec![0x1a, 0x86, 0x1b, 0x86, 0x95, 0xac],
        2,
        2,
        ordered,
    );
    check_fp(
        "fcmpg_ord",
        "(II)I",
        vec![0x1a, 0x86, 0x1b, 0x86, 0x96, 0xac],
        2,
        2,
        ordered,
    );
}

#[test]
fn ir_vs_singlepass_dcmp_ordered() {
    //   iload_0; i2d; iload_1; i2d; dcmp{l,g}; ireturn
    let ordered = &[
        (vec![1i64, 2], -1i32),
        (vec![2, 1], 1),
        (vec![7, 7], 0),
        (vec![-9, 4], -1),
        (vec![4, -9], 1),
    ];
    check_fp(
        "dcmpl_ord",
        "(II)I",
        vec![0x1a, 0x87, 0x1b, 0x87, 0x97, 0xac],
        2,
        2,
        ordered,
    );
    check_fp(
        "dcmpg_ord",
        "(II)I",
        vec![0x1a, 0x87, 0x1b, 0x87, 0x98, 0xac],
        2,
        2,
        ordered,
    );
}

#[test]
fn ir_vs_singlepass_fcmp_nan() {
    // NaN is synthesized as 0.0f/0.0f (fconst_0 fconst_0 fdiv). fcmpl → -1 and
    // fcmpg → +1 for a NaN in either position.
    // int f() { return (0/0f) fcmpl 1f; }  — NaN as LEFT operand
    check_fp(
        "fcmpl_nan_lhs",
        "()I",
        vec![0x0b, 0x0b, 0x6e, 0x0c, 0x95, 0xac],
        0,
        0,
        &[(vec![], -1)],
    );
    // int f() { return (0/0f) fcmpg 1f; }
    check_fp(
        "fcmpg_nan_lhs",
        "()I",
        vec![0x0b, 0x0b, 0x6e, 0x0c, 0x96, 0xac],
        0,
        0,
        &[(vec![], 1)],
    );
    // int f() { return 1f fcmpl (0/0f); }  — NaN as RIGHT operand
    check_fp(
        "fcmpl_nan_rhs",
        "()I",
        vec![0x0c, 0x0b, 0x0b, 0x6e, 0x95, 0xac],
        0,
        0,
        &[(vec![], -1)],
    );
    // int f() { return 1f fcmpg (0/0f); }
    check_fp(
        "fcmpg_nan_rhs",
        "()I",
        vec![0x0c, 0x0b, 0x0b, 0x6e, 0x96, 0xac],
        0,
        0,
        &[(vec![], 1)],
    );
}

#[test]
fn ir_vs_singlepass_dcmp_nan() {
    // NaN as 0.0/0.0 (dconst_0 dconst_0 ddiv). dcmpl → -1, dcmpg → +1.
    check_fp(
        "dcmpl_nan_lhs",
        "()I",
        vec![0x0e, 0x0e, 0x6f, 0x0f, 0x97, 0xac],
        0,
        0,
        &[(vec![], -1)],
    );
    check_fp(
        "dcmpg_nan_lhs",
        "()I",
        vec![0x0e, 0x0e, 0x6f, 0x0f, 0x98, 0xac],
        0,
        0,
        &[(vec![], 1)],
    );
    check_fp(
        "dcmpl_nan_rhs",
        "()I",
        vec![0x0f, 0x0e, 0x0e, 0x6f, 0x97, 0xac],
        0,
        0,
        &[(vec![], -1)],
    );
    check_fp(
        "dcmpg_nan_rhs",
        "()I",
        vec![0x0f, 0x0e, 0x0e, 0x6f, 0x98, 0xac],
        0,
        0,
        &[(vec![], 1)],
    );
}

// ── double RETURNS — method-level dreturn + D call returns (inc 32) ──────────
//
// A `double` result rides RAX as clean 64-bit bits (the JIT i64 return ABI; the
// interpreter reads `result as u64` → `f64::from_bits`). The `dreturn` builder
// arm + `Op::Return`'s `load_to_rax` need no XMM return marshalling, and a `D`
// call result's `-0.0`/`i64::MIN` bit collision is disambiguated by the item-1
// `dispatch_threw` peek (the call-site check matches `IrType::Double`).

#[test]
fn ir_vs_singlepass_double_return() {
    // static double f(int x) { return (double)x + 1.0; }
    //   iload_0; i2d; dconst_1; dadd; dreturn
    let code = vec![0x1a, 0x87, 0x0f, 0x63, 0xaf];
    let cm = cached("dret", "(I)D", code, 1, 1);
    let helpers = dummy_helpers();
    let ir = compile_fp_opt(&cm, &helpers, true)
        .expect("IR compile of double-returning method (FP gate)");
    let sp = compile_fp_opt(&cm, &helpers, false)
        .expect("single-pass compile of double-returning method");
    for x in [0i64, 5, -3, 100, -100, i32::MIN as i64] {
        // SAFETY: both bodies take one int arg and return a double whose bits
        // ride RAX; `try_call` returns that i64 (the f64 bit pattern).
        let r_ir = f64::from_bits(unsafe { ir.try_call(&[x]) }.unwrap() as u64);
        let r_sp = f64::from_bits(unsafe { sp.try_call(&[x]) }.unwrap() as u64);
        let expected = x as f64 + 1.0;
        assert_eq!(
            r_ir.to_bits(),
            expected.to_bits(),
            "IR double-return for x={x}"
        );
        assert_eq!(
            r_sp.to_bits(),
            expected.to_bits(),
            "single-pass double-return for x={x}"
        );
    }
}

#[test]
fn ir_vs_singlepass_invokestatic_double_return() {
    // static double f(int x) { return g(x) + 1.0; }   // g:(I)D
    //   iload_0; invokestatic #2; dconst_1; dadd; dreturn
    // The caller consumes a `double`-returning Op::Call result — exercises the
    // `static_call_shape` D-return admission + `IrType::Double` call-result
    // typing + the item-1 sentinel disambiguation on a D call result.
    unsafe extern "C" fn dret_dispatch(_vm: i64, _i: i64, args_ptr: i64, _n: i64) -> i64 {
        // g(x) = (double)x * 0.5, returned as raw f64 bits in RAX.
        let x = unsafe { *(args_ptr as *const i64) } as i32;
        ((x as f64) * 0.5).to_bits() as i64
    }
    unsafe extern "C" fn never_threw() -> i64 {
        0
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = dret_dispatch as *const () as usize;
    helpers.dispatch_threw = never_threw as *const () as usize;
    let code = vec![0x1a, 0xb8, 0x00, 0x02, 0x0f, 0x63, 0xaf];
    let cm = cached("f", "(I)D", code, 1, 1);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(I)D".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch_fp(&cm, &helpers, &resolver)
        .expect("IR compile of double-call-returning method");
    assert!(ir.needs_context());
    let dummy_vm = [0u8; 64];
    for x in [4i64, 5, -6, 0, 1000] {
        let r = f64::from_bits(
            unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[x]) }.unwrap() as u64,
        );
        let expected = (x as f64) * 0.5 + 1.0;
        assert_eq!(r.to_bits(), expected.to_bits(), "D call-return for x={x}");
    }
}

// ── float RETURNS — method-level freturn + F call returns (inc 33) ───────────
//
// A `float` result rides the LOW 32 of RAX; consumers read only the low 32
// (interpreter `result as u32`; downstream `MOVSS`), so stale upper bits are
// harmless EXCEPT the `+0.0f`/`i64::MIN`-bits collision on a call result, which
// the call-site `dispatch_threw` peek (now `IrType::Float`) catches.

#[test]
fn ir_vs_singlepass_float_return() {
    // static float f(int x) { return (float)x + 1.0f; }
    //   iload_0; i2f; fconst_1; fadd; freturn
    let code = vec![0x1a, 0x86, 0x0c, 0x62, 0xae];
    let cm = cached("fret", "(I)F", code, 1, 1);
    let helpers = dummy_helpers();
    let ir = compile_fp_opt(&cm, &helpers, true)
        .expect("IR compile of float-returning method (FP gate)");
    let sp = compile_fp_opt(&cm, &helpers, false)
        .expect("single-pass compile of float-returning method");
    for x in [0i64, 5, -3, 100, -100, i32::MIN as i64] {
        // float bits ride the low 32 of RAX (stale upper bits are masked).
        let r_ir = f32::from_bits(unsafe { ir.try_call(&[x]) }.unwrap() as u32);
        let r_sp = f32::from_bits(unsafe { sp.try_call(&[x]) }.unwrap() as u32);
        let expected = x as f32 + 1.0f32;
        assert_eq!(
            r_ir.to_bits(),
            expected.to_bits(),
            "IR float-return for x={x}"
        );
        assert_eq!(
            r_sp.to_bits(),
            expected.to_bits(),
            "single-pass float-return for x={x}"
        );
    }
}

#[test]
fn ir_vs_singlepass_invokestatic_float_return() {
    // static float f(int x) { return g(x) + 1.0f; }   // g:(I)F
    //   iload_0; invokestatic #2; fconst_1; fadd; freturn
    unsafe extern "C" fn fret_dispatch(_vm: i64, _i: i64, args_ptr: i64, _n: i64) -> i64 {
        let x = unsafe { *(args_ptr as *const i64) } as i32;
        // g(x) = (float)x * 0.5f, returned as float bits.
        ((x as f32) * 0.5f32).to_bits() as i64
    }
    unsafe extern "C" fn never_threw() -> i64 {
        0
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = fret_dispatch as *const () as usize;
    helpers.dispatch_threw = never_threw as *const () as usize;
    let code = vec![0x1a, 0xb8, 0x00, 0x02, 0x0c, 0x62, 0xae];
    let cm = cached("f", "(I)F", code, 1, 1);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(I)F".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch_fp(&cm, &helpers, &resolver)
        .expect("IR compile of float-call-returning method");
    let dummy_vm = [0u8; 64];
    for x in [4i64, 5, -6, 0, 1000] {
        let r = f32::from_bits(
            unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[x]) }.unwrap() as u32,
        );
        let expected = (x as f32) * 0.5f32 + 1.0f32;
        assert_eq!(r.to_bits(), expected.to_bits(), "F call-return for x={x}");
    }
}

#[test]
fn ir_vs_singlepass_float_return_min_bits_collision() {
    // The float collision: a `+0.0f` call result whose RAX bits == i64::MIN (low
    // 32 = 0 = +0.0f, upper 32 = 0x8000_0000 stale) must NOT be misread as the
    // deopt sentinel. `dispatch_threw` reports no signal, the caller keeps it, and
    // the low 32 (+0.0f) flows on → +0.0f + 1.0f = 1.0f. (i64::MIN has low-63 = 0,
    // so ONLY +0.0f can ever collide for a float.)
    unsafe extern "C" fn min_bits_dispatch(_vm: i64, _i: i64, _a: i64, _n: i64) -> i64 {
        i64::MIN // low 32 = 0 = +0.0f bits; upper 32 = stale 0x8000_0000
    }
    unsafe extern "C" fn never_threw() -> i64 {
        0
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = min_bits_dispatch as *const () as usize;
    helpers.dispatch_threw = never_threw as *const () as usize;
    // static float f(int x) { return g(x) + 1.0f; }
    let code = vec![0x1a, 0xb8, 0x00, 0x02, 0x0c, 0x62, 0xae];
    let cm = cached("f", "(I)F", code, 1, 1);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(I)F".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch_fp(&cm, &helpers, &resolver).expect("compile");
    let dummy_vm = [0u8; 64];
    let r = f32::from_bits(
        unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[7]) }.unwrap() as u32,
    );
    assert_eq!(
        r.to_bits(),
        1.0f32.to_bits(),
        "a +0.0f call-return whose RAX bits == i64::MIN must be kept (→ +0.0f + 1.0f = 1.0f), not bailed"
    );
}

// ── FP PARAMS + D/F call-ARGS (inc 34) ──────────────────────────────────────
//
// The VM uses the compact all-GPR i64 ABI: an FP param/arg arrives as bits in an
// INTEGER register (`to_bits() as i64`), NOT XMM. The prologue stores it to the
// param slot like any other param; `set_param_types` types it Float/Double (cat-2
// two-slot layout for `D`), so a `dload`/`fload` reads it via `fp_load`. So a
// method with an FP SIGNATURE (not just FP-internal) now takes the IR path.

#[test]
fn ir_vs_singlepass_double_param() {
    // static double f(double a, int b) { return a + (double)b; }
    //   dload_0; iload_2; i2d; dadd; dreturn   (a: D @ jvm 0-1, b: I @ jvm 2)
    let code = vec![0x26, 0x1c, 0x87, 0x63, 0xaf];
    let cm = cached("dparam", "(DI)D", code, 3, 2);
    let helpers = dummy_helpers();
    let ir = compile_fp_opt(&cm, &helpers, true).expect("IR double-param");
    let sp = compile_fp_opt(&cm, &helpers, false).expect("single-pass double-param");
    for (a, b) in [(1.5f64, 2i64), (-3.25, 7), (1e10, -5), (0.0, 0)] {
        let args = [a.to_bits() as i64, b];
        let r_ir = f64::from_bits(unsafe { ir.try_call(&args) }.unwrap() as u64);
        let r_sp = f64::from_bits(unsafe { sp.try_call(&args) }.unwrap() as u64);
        let expected = a + b as f64;
        assert_eq!(
            r_ir.to_bits(),
            expected.to_bits(),
            "IR double-param a={a},b={b}"
        );
        assert_eq!(
            r_sp.to_bits(),
            expected.to_bits(),
            "single-pass double-param a={a},b={b}"
        );
    }
}

#[test]
fn ir_vs_singlepass_float_param() {
    // static float f(float a, int b) { return a + (float)b; }
    //   fload_0; iload_1; i2f; fadd; freturn   (a: F @ jvm 0, b: I @ jvm 1)
    let code = vec![0x22, 0x1b, 0x86, 0x62, 0xae];
    let cm = cached("fparam", "(FI)F", code, 2, 2);
    let helpers = dummy_helpers();
    let ir = compile_fp_opt(&cm, &helpers, true).expect("IR float-param");
    let sp = compile_fp_opt(&cm, &helpers, false).expect("single-pass float-param");
    for (a, b) in [(1.5f32, 2i64), (-3.25, 7), (100.0, -5), (0.0, 0)] {
        let args = [a.to_bits() as i64, b];
        let r_ir = f32::from_bits(unsafe { ir.try_call(&args) }.unwrap() as u32);
        let r_sp = f32::from_bits(unsafe { sp.try_call(&args) }.unwrap() as u32);
        let expected = a + b as f32;
        assert_eq!(
            r_ir.to_bits(),
            expected.to_bits(),
            "IR float-param a={a},b={b}"
        );
        assert_eq!(
            r_sp.to_bits(),
            expected.to_bits(),
            "single-pass float-param a={a},b={b}"
        );
    }
}

#[test]
fn ir_vs_singlepass_invokestatic_fp_args() {
    // static double f(double a, float b) { return g(a, b); }   // g:(DF)D
    //   dload_0; fload_2; invokestatic #2; dreturn   (a: D @ 0-1, b: F @ 2)
    // Exercises a `double` arg + a `float` arg through `Op::Call` (marshalled as
    // bits to the staging region) + the FP params of the caller + a `D` call return.
    unsafe extern "C" fn dfarg_dispatch(_vm: i64, _i: i64, args_ptr: i64, n: i64) -> i64 {
        assert_eq!(n, 2, "dfarg_dispatch expects (double, float)");
        let p = args_ptr as *const i64;
        let a = f64::from_bits(unsafe { *p } as u64); // arg0 = double (full 64 bits)
        let b = f32::from_bits(unsafe { *p.add(1) } as u32); // arg1 = float (low 32)
        (a + b as f64).to_bits() as i64
    }
    unsafe extern "C" fn never_threw() -> i64 {
        0
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = dfarg_dispatch as *const () as usize;
    helpers.dispatch_threw = never_threw as *const () as usize;
    let code = vec![0x26, 0x24, 0xb8, 0x00, 0x02, 0xaf];
    let cm = cached("f", "(DF)D", code, 3, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(DF)D".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch_fp(&cm, &helpers, &resolver)
        .expect("IR compile of FP-call-args method");
    assert!(ir.needs_context());
    let dummy_vm = [0u8; 64];
    for (a, b) in [(1.5f64, 2.5f32), (-3.0, 0.25), (1e9, -1.0), (0.0, 0.0)] {
        let args = [a.to_bits() as i64, b.to_bits() as i64];
        let r = f64::from_bits(
            unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &args) }.unwrap() as u64,
        );
        let expected = a + b as f64;
        assert_eq!(r.to_bits(), expected.to_bits(), "FP call-args a={a},b={b}");
    }
}

#[test]
fn ir_vs_singlepass_fp_constants() {
    // int f(int a) { return (int)((float)a + 2.0f); }  — fconst_2
    check_fp(
        "fconst2",
        "(I)I",
        vec![0x1a, 0x86, 0x0d, 0x62, 0x8b, 0xac],
        1,
        1,
        &[(vec![5], 7), (vec![-3], -1), (vec![100], 102)],
    );
    // int f(int a) { return (int)((double)a + 1.0); }  — dconst_1
    check_fp(
        "dconst1",
        "(I)I",
        vec![0x1a, 0x87, 0x0f, 0x63, 0x8e, 0xac],
        1,
        1,
        &[(vec![5], 6), (vec![-3], -2)],
    );
}

#[test]
fn ir_vs_singlepass_fp_fp_conversions() {
    // int f(int a) { return (int)(double)(float)a; }  — i2f; f2d; d2i
    check_fp(
        "f2d",
        "(I)I",
        vec![0x1a, 0x86, 0x8d, 0x8e, 0xac],
        1,
        1,
        &[(vec![5], 5), (vec![-3], -3), (vec![16_777_216], 16_777_216)],
    );
    // int f(int a) { double d = a; float fv = (float)d; return (int)fv; }  — d2f
    check_fp(
        "d2f",
        "(I)I",
        vec![0x1a, 0x87, 0x90, 0x8b, 0xac],
        1,
        1,
        &[(vec![5], 5), (vec![-100], -100)],
    );
}

#[test]
fn ir_vs_singlepass_long_fp_conversions() {
    // int f(int a) { long L = a; float fv = L; return (int)fv; }  — i2l; l2f; f2i
    check_fp(
        "l2f",
        "(I)I",
        vec![0x1a, 0x85, 0x89, 0x8b, 0xac],
        1,
        1,
        &[(vec![1000], 1000), (vec![-7], -7)],
    );
    // int f(int a) { float fv = a; long L = (long)fv; return (int)L; }  — i2f; f2l; l2i
    check_fp(
        "f2l",
        "(I)I",
        vec![0x1a, 0x86, 0x8c, 0x88, 0xac],
        1,
        1,
        &[(vec![1000], 1000), (vec![-7], -7)],
    );
    // int f(int a) { long L=a; double d=L; long r=(long)d; return (int)r; }
    //   i2l; l2d; d2l; l2i
    check_fp(
        "l2d_d2l",
        "(I)I",
        vec![0x1a, 0x85, 0x8a, 0x8f, 0x88, 0xac],
        1,
        1,
        &[(vec![1000], 1000), (vec![-7], -7), (vec![123_456], 123_456)],
    );
}

#[test]
fn ir_vs_singlepass_double_locals() {
    // int f(int a) { double x = a; double y = x*x; return (int)(y + x); }
    //   iload_0; i2d; dstore_1; dload_1; dload_1; dmul; dstore_3; dload_3;
    //   dload_1; dadd; d2i; ireturn   — exercises dstore_1/3 + dload_1/3 (cat-2
    //   locals: x at slots 1-2, y at slots 3-4).
    check_fp(
        "dlocals",
        "(I)I",
        vec![
            0x1a, 0x87, 0x48, 0x27, 0x27, 0x6b, 0x4a, 0x29, 0x27, 0x63, 0x8e, 0xac,
        ],
        5,
        1,
        &[(vec![5], 30), (vec![10], 110), (vec![-3], 6), (vec![0], 0)],
    );
}

#[test]
fn ir_vs_singlepass_wide_fp_load_store() {
    // int f(int a) { float fv = a; return (int)fv; }  with fv at slot 5, forcing
    // the wide fstore/fload forms: iload_0; i2f; fstore 5; fload 5; f2i; ireturn.
    check_fp(
        "wide_fls",
        "(I)I",
        vec![0x1a, 0x86, 0x38, 0x05, 0x17, 0x05, 0x8b, 0xac],
        6,
        1,
        &[(vec![5], 5), (vec![-100], -100), (vec![42], 42)],
    );
}

#[test]
fn ir_vs_singlepass_fp_nan_to_zero() {
    // int f() { float x = 0.0f; return (int)(x / x); }  — 0.0/0.0 = NaN → 0.
    //   fconst_0; fconst_0; fdiv; f2i; ireturn
    check_fp(
        "fnan",
        "()I",
        vec![0x0b, 0x0b, 0x6e, 0x8b, 0xac],
        0,
        0,
        &[(vec![], 0)],
    );
    // int f() { double x = 0.0; return (int)(x / x); }  — dconst_0; dconst_0;
    //   ddiv; d2i; ireturn
    check_fp(
        "dnan",
        "()I",
        vec![0x0e, 0x0e, 0x6f, 0x8e, 0xac],
        0,
        0,
        &[(vec![], 0)],
    );
}

#[test]
fn ir_vs_singlepass_fp_to_long_fixup() {
    // Exercise the 64-bit (`is_long`) NaN/overflow fixup branch — the long-result
    // conversions f2l/d2l — across all three arms (NaN→0, +inf→LONG_MAX,
    // -inf→LONG_MIN). The result is returned as `(int)(long)…` = the low 32 bits
    // of the saturated long: LONG_MAX (0x7FFF…FF) → -1, LONG_MIN (0x8000…0) → 0,
    // 0L → 0. Inf/NaN are built deterministically from `x/0` (no float rounding).

    // (int)(long)(1.0/0.0) = (int)(+inf) → (int)LONG_MAX = -1.   d2l, +overflow
    check_fp(
        "d2l_pinf",
        "()I",
        vec![0x0f, 0x0e, 0x6f, 0x8f, 0x88, 0xac], // dconst_1; dconst_0; ddiv; d2l; l2i; ireturn
        0,
        0,
        &[(vec![], -1)],
    );
    // (int)(long)(-1.0/0.0) = (int)(-inf) → (int)LONG_MIN = 0.   d2l, -overflow
    check_fp(
        "d2l_ninf",
        "()I",
        vec![0x0f, 0x77, 0x0e, 0x6f, 0x8f, 0x88, 0xac], // dconst_1; dneg; dconst_0; ddiv; d2l; l2i; ireturn
        0,
        0,
        &[(vec![], 0)],
    );
    // (int)(long)(0.0/0.0) = (int)(long)NaN = 0.   d2l, NaN
    check_fp(
        "d2l_nan",
        "()I",
        vec![0x0e, 0x0e, 0x6f, 0x8f, 0x88, 0xac], // dconst_0; dconst_0; ddiv; d2l; l2i; ireturn
        0,
        0,
        &[(vec![], 0)],
    );
    // (int)(long)(1.0f/0.0f) = (int)(+inf) → (int)LONG_MAX = -1.   f2l, +overflow
    check_fp(
        "f2l_pinf",
        "()I",
        vec![0x0c, 0x0b, 0x6e, 0x8c, 0x88, 0xac], // fconst_1; fconst_0; fdiv; f2l; l2i; ireturn
        0,
        0,
        &[(vec![], -1)],
    );
}

// ── Backend-routing guards for the fib44 self-recursion fix ──
//
// With the direct-call opt-out forced for this test, a SELF-RECURSIVE
// wide-return (J/D/F) call must bail the method to single-pass, NOT lower to an
// IR `Op::Call` routed through the generic dispatch helper per call.
// The gate is keyed on `CompiledMethod::used_ir_backend` (true = optimizing IR
// pipeline produced the body; false = single-pass, incl. a bail). These pin the
// gate's behaviour AND its specificity (it fires ONLY for self-recursive J/D/F).

#[test]
fn selfrec_long_return_bails_to_singlepass() {
    struct ResetOverride;
    impl Drop for ResetOverride {
        fn drop(&mut self) {
            cratonvm_jit::__set_selfrec_direct_override(None);
        }
    }
    let _guard = ResetOverride;
    cratonvm_jit::__set_selfrec_direct_override(Some(false));

    // static long fib(int n) { return n < 2 ? n : fib(n-1) + fib(n-2); }  // (I)J
    let helpers = dummy_helpers();
    let code = vec![
        0x1a, 0x04, 0xa3, 0x00, 0x06, // iload_0; iconst_1; if_icmpgt 8
        0x1a, 0x85, 0xad, // iload_0; i2l; lreturn
        0x1a, 0x04, 0x64, 0xb8, 0x00, 0x02, // iload_0; iconst_1; isub; invokestatic #2 (self)
        0x1a, 0x05, 0x64, 0xb8, 0x00, 0x02, // iload_0; iconst_2; isub; invokestatic #2 (self)
        0x61, 0xad, // ladd; lreturn
    ];
    let cm = cached("fib", "(I)J", code, 1, 1);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        // CP #2 resolves to THIS method (self-recursion). `cached` uses class
        // "Corpus", method = the given name, descriptor = the given descriptor.
        if cp == 2 {
            Some(("Corpus".into(), "fib".into(), "(I)J".into()))
        } else {
            None
        }
    };
    let compiled =
        compile_with_dispatch(&cm, &helpers, &resolver).expect("compiles (via single-pass)");
    assert!(
        !compiled.used_ir_backend,
        "fib44 regression guard: a self-recursive long-returning method must bail to \
         single-pass (direct self-call), not the IR Op::Call/jit_invoke_dispatch path"
    );
}

#[test]
fn selfrec_int_return_uses_ir() {
    // static int f(int n) { return n < 2 ? n : f(n-1) + f(n-2); }  // (I)I
    // The gate is wide-return-specific: an INT self-recursive call is unaffected
    // and still lowers to the IR Op::Call path (pre-existing behaviour).
    let helpers = dummy_helpers();
    let code = vec![
        0x1a, 0x04, 0xa3, 0x00, 0x05, // iload_0; iconst_1; if_icmpgt 7
        0x1a, 0xac, // iload_0; ireturn
        0x1a, 0x04, 0x64, 0xb8, 0x00, 0x02, // iload_0; iconst_1; isub; invokestatic #2 (self)
        0x1a, 0x05, 0x64, 0xb8, 0x00, 0x02, // iload_0; iconst_2; isub; invokestatic #2 (self)
        0x60, 0xac, // iadd; ireturn
    ];
    let cm = cached("f", "(I)I", code, 1, 1);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("Corpus".into(), "f".into(), "(I)I".into()))
        } else {
            None
        }
    };
    let compiled = compile_with_dispatch(&cm, &helpers, &resolver).expect("compiles");
    assert!(
        compiled.used_ir_backend,
        "an int (non-wide) self-recursive call is not gated and should use the IR backend"
    );
}

#[test]
fn crossmethod_long_return_uses_ir() {
    // static long f(int n) { return g(n); }   // g:(I)J, NOT self-recursive
    // Confirms the gate is SELF-recursion specific: a cross-method long-returning
    // call is the intended inc-29 unblock and still lowers to IR Op::Call.
    let helpers = dummy_helpers();
    let code = vec![0x1a, 0xb8, 0x00, 0x02, 0xad]; // iload_0; invokestatic #2 (g); lreturn
    let cm = cached("f", "(I)J", code, 1, 1);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        // CP #2 resolves to a DIFFERENT method `g` (not the compiling method `f`).
        if cp == 2 {
            Some(("Corpus".into(), "g".into(), "(I)J".into()))
        } else {
            None
        }
    };
    let compiled = compile_with_dispatch(&cm, &helpers, &resolver).expect("compiles");
    assert!(
        compiled.used_ir_backend,
        "a cross-method long-returning call must NOT be gated (intended IR unblock)"
    );
}

#[test]
fn selfrec_int_direct_call_executes_correctly() {
    // Integer-returning self-recursion is the common QuickBench fib shape.
    // With the direct-call gate on it must stay on IR without routing every
    // recursive edge through jit_invoke_dispatch.
    struct ResetOverride;
    impl Drop for ResetOverride {
        fn drop(&mut self) {
            cratonvm_jit::__set_selfrec_direct_override(None);
        }
    }
    let _guard = ResetOverride;
    cratonvm_jit::__set_selfrec_direct_override(Some(true));

    let helpers = dummy_helpers();
    // static int fib(int n) { return n < 2 ? n : fib(n-1) + fib(n-2); }
    let code = vec![
        0x1a, 0x04, 0xa3, 0x00, 0x05, // iload_0; iconst_1; if_icmpgt 7
        0x1a, 0xac, // iload_0; ireturn
        0x1a, 0x04, 0x64, 0xb8, 0x00, 0x02, // iload_0; iconst_1; isub; invokestatic #2
        0x1a, 0x05, 0x64, 0xb8, 0x00, 0x02, // iload_0; iconst_2; isub; invokestatic #2
        0x60, 0xac, // iadd; ireturn
    ];
    let cm = cached("fib", "(I)I", code, 1, 1);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("Corpus".into(), "fib".into(), "(I)I".into()))
        } else {
            None
        }
    };
    let compiled = compile_with_dispatch(&cm, &helpers, &resolver)
        .expect("self-recursive int compiles on the IR direct-call path");
    assert!(
        compiled.used_ir_backend,
        "integer self-recursion should use the IR direct-call backend"
    );

    let dummy_vm = [0u8; 64];
    for (n, expect) in [
        (0i64, 0i64),
        (1, 1),
        (2, 1),
        (5, 5),
        (10, 55),
        (20, 6765),
        (30, 832040),
    ] {
        let r = unsafe { compiled.try_call_with_context(dummy_vm.as_ptr() as i64, &[n]) }
            .unwrap_or_else(|e| panic!("call fib({n}): {e:?}"));
        assert_eq!(r, expect, "fib({n}) via IR direct self-call");
    }
}

#[test]
fn selfrec_long_direct_call_executes_correctly() {
    // fib44-fix follow-up: with CRATONVM_JIT_IR_SELFREC_DIRECT on, a self-recursive
    // long method stays on the IR pipeline but its recursive call is a DIRECT call
    // to its own entry (not jit_invoke_dispatch). Verify it (a) takes the IR
    // backend and (b) recurses correctly end-to-end (the real codegen arbiter).
    //
    // Reset guard: the override is thread-local and the test harness reuses
    // threads, so a leak (even on panic) would flip the flag for a later test on
    // this thread (e.g. `selfrec_long_return_bails_to_singlepass`). Drop restores.
    struct ResetOverride;
    impl Drop for ResetOverride {
        fn drop(&mut self) {
            cratonvm_jit::__set_selfrec_direct_override(None);
        }
    }
    let _guard = ResetOverride;
    cratonvm_jit::__set_selfrec_direct_override(Some(true));

    let helpers = dummy_helpers();
    // static long fib(int n) { return n < 2 ? n : fib(n-1) + fib(n-2); }
    let code = vec![
        0x1a, 0x04, 0xa3, 0x00, 0x06, // iload_0; iconst_1; if_icmpgt 8
        0x1a, 0x85, 0xad, // iload_0; i2l; lreturn
        0x1a, 0x04, 0x64, 0xb8, 0x00, 0x02, // iload_0; iconst_1; isub; invokestatic #2 (self)
        0x1a, 0x05, 0x64, 0xb8, 0x00, 0x02, // iload_0; iconst_2; isub; invokestatic #2 (self)
        0x61, 0xad, // ladd; lreturn
    ];
    let cm = cached("fib", "(I)J", code, 1, 1);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("Corpus".into(), "fib".into(), "(I)J".into()))
        } else {
            None
        }
    };
    let compiled = compile_with_dispatch(&cm, &helpers, &resolver)
        .expect("self-recursive long compiles on the IR direct-call path");
    assert!(
        compiled.used_ir_backend,
        "with the flag on, a self-recursive long method stays on the IR backend (direct self-call)"
    );

    // `vm_ptr` is only forwarded to recursive calls, never dereferenced by fib —
    // a dummy buffer pointer suffices.
    let dummy_vm = [0u8; 64];
    for (n, expect) in [
        (0i64, 0i64),
        (1, 1),
        (2, 1),
        (5, 5),
        (10, 55),
        (20, 6765),
        (30, 832040),
    ] {
        let r = unsafe { compiled.try_call_with_context(dummy_vm.as_ptr() as i64, &[n]) }
            .unwrap_or_else(|e| panic!("call fib({n}): {e:?}"));
        assert_eq!(r, expect, "fib({n}) via IR direct self-call");
    }
}

// ---------------------------------------------------------------------------
// IR direct-call lowering (perf(jit): direct cross-method calls in the IR)
//
// The IR path historically routed EVERY invoke through `jit_invoke_dispatch`,
// which is why `ir_compatible` capped `invoke_ops` at 5: an "optimizing"
// recompile of a call-heavy method was a net regression versus the single-pass
// body, which has always bound a statically-resolved callee with a raw `CALL`.
// The lowering below closes that gap for `invokestatic` / non-`<init>`
// `invokespecial`. These tests supply a `callee_compiler` whose returned "entry"
// is a real `extern "C"` stub, so the generated code actually calls it — proving
// the entry ABI (vm_ptr placement, argument register order, result in RAX) and
// the exception sentinel end to end.
//
// `invoke_dispatch` is deliberately wired to a DIFFERENT observable function in
// each test: if the site fell back to helper dispatch the result would differ, so
// each assertion is also a proof that the direct edge was taken.
// ---------------------------------------------------------------------------

/// A "compiled callee" that takes the hidden VM context pointer (needs_context
/// == true): `(vm_ptr, a, b) -> a * 1000 + b`.
unsafe extern "C" fn direct_callee_ctx(_vm: i64, a: i64, b: i64) -> i64 {
    (a as i32 as i64) * 1000 + (b as i32 as i64)
}

/// A "compiled callee" that does NOT take a context pointer (needs_context ==
/// false), so its FIRST register argument is the first Java argument:
/// `(a, b) -> a * 1000 + b + 7`.
unsafe extern "C" fn direct_callee_noctx(a: i64, b: i64) -> i64 {
    (a as i32 as i64) * 1000 + (b as i32 as i64) + 7
}

/// A "compiled callee" that threw: returns the `i64::MIN` deopt sentinel.
unsafe extern "C" fn direct_callee_throws(_vm: i64, _a: i64, _b: i64) -> i64 {
    i64::MIN
}

/// `compile_with_dispatch` plus a `callee_compiler` — the resolver the IR
/// eligibility loop consults to eagerly compile a statically-bound callee and
/// obtain `(entry, needs_context)` for the direct `CALL`.
fn compile_with_direct_callee(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    invoke_resolver: &dyn Fn(u16) -> Option<(String, String, String)>,
    callee_compiler: &dyn Fn(&str, &str, &str) -> Option<(usize, bool)>,
) -> Option<CompiledMethod> {
    try_compile(
        cm,
        None,
        None,
        None,
        Some(invoke_resolver),
        Some(callee_compiler),
        None,
        None,
        None,
        None,
        helpers,
        None,
        None,
        None,
        None,
        true,  // optimize (C2 / IR pipeline)
        true,  // ir_emit_calls
        true,  // ir_emit_special_calls
        true,  // ir_emit_long
        true,  // ir_emit_virtual_calls
        false, // ir_emit_fp
        None,
    )
}

#[test]
fn ir_direct_call_static_with_context_executes_correctly() {
    // static int f(int a, int b) { return g(a, b) * 2; }
    //   iload_0; iload_1; invokestatic #2; iconst_2; imul; ireturn
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = positional_dispatch as *const () as usize;
    let code = vec![0x1a, 0x1b, 0xb8, 0x00, 0x02, 0x05, 0x68, 0xac];
    let cm = cached("f", "(II)I", code, 2, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(II)I".into()))
        } else {
            None
        }
    };
    let entry = direct_callee_ctx as *const () as usize;
    let callee_compiler = |c: &str, m: &str, d: &str| -> Option<(usize, bool)> {
        if (c, m, d) == ("pkg/Helper", "g", "(II)I") {
            Some((entry, true))
        } else {
            None
        }
    };
    let ir = compile_with_direct_callee(&cm, &helpers, &resolver, &callee_compiler)
        .expect("IR compile with a direct static call");
    assert!(ir.used_ir_backend, "must stay on the IR backend");
    assert!(
        ir._direct_callee_entries.contains(&entry),
        "the baked callee entry must be recorded for keep-alive + invalidation"
    );
    let dummy_vm = [0u8; 64];
    for (a, b) in [(3i64, 4i64), (0, 0), (-1, 5), (12, -7)] {
        let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[a, b]) }
            .unwrap_or_else(|e| panic!("call f({a},{b}): {e:?}"));
        let expected = ((a as i32).wrapping_mul(1000).wrapping_add(b as i32)).wrapping_mul(2);
        // Compared as `i32`, matching the `check` helper: the IR keeps an int
        // result in its frame slot without guaranteeing upper-32 sign extension
        // (`Op::Mul` Int is a 32-bit `IMUL EAX, ECX`), and the VM narrows an `I`
        // return to i32. Pre-existing convention, unrelated to call lowering.
        assert_eq!(
            r as i32, expected,
            "f({a},{b}) must go through the DIRECT callee (helper dispatch would              have produced the positional_dispatch value instead)"
        );
    }
}

#[test]
fn ir_direct_call_static_without_context_executes_correctly() {
    // Same shape, but the callee reports `needs_context == false`, so the first
    // Java argument occupies abi[0] rather than abi[1]. A mis-shifted marshalling
    // would pass the VM pointer as `a`.
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = positional_dispatch as *const () as usize;
    let code = vec![0x1a, 0x1b, 0xb8, 0x00, 0x02, 0xac];
    let cm = cached("f", "(II)I", code, 2, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(II)I".into()))
        } else {
            None
        }
    };
    let entry = direct_callee_noctx as *const () as usize;
    let callee_compiler = |c: &str, m: &str, d: &str| -> Option<(usize, bool)> {
        if (c, m, d) == ("pkg/Helper", "g", "(II)I") {
            Some((entry, false))
        } else {
            None
        }
    };
    let ir = compile_with_direct_callee(&cm, &helpers, &resolver, &callee_compiler)
        .expect("IR compile with a context-free direct callee");
    let dummy_vm = [0u8; 64];
    for (a, b) in [(3i64, 4i64), (-2, 9), (100, 200)] {
        let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[a, b]) }
            .unwrap_or_else(|e| panic!("call f({a},{b}): {e:?}"));
        let expected = (a as i32)
            .wrapping_mul(1000)
            .wrapping_add(b as i32)
            .wrapping_add(7);
        assert_eq!(
            r as i32, expected,
            "context-free direct call for f({a},{b})"
        );
    }
}

#[test]
fn ir_direct_call_exception_sentinel_bails() {
    // A directly-called callee that threw returns `i64::MIN`. The caller must
    // detect the sentinel and bail (returning it unchanged so the VM takes the
    // pending exception) instead of using it as the call's result — exactly the
    // protocol the helper-dispatch path follows.
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = positional_dispatch as *const () as usize;
    // static int f(int a, int b) { return g(a, b) * 2; } — the `* 2` would be
    // visible in the result if the bail did not happen.
    let code = vec![0x1a, 0x1b, 0xb8, 0x00, 0x02, 0x05, 0x68, 0xac];
    let cm = cached("f", "(II)I", code, 2, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(II)I".into()))
        } else {
            None
        }
    };
    let entry = direct_callee_throws as *const () as usize;
    let callee_compiler =
        |_c: &str, _m: &str, _d: &str| -> Option<(usize, bool)> { Some((entry, true)) };
    let ir =
        compile_with_direct_callee(&cm, &helpers, &resolver, &callee_compiler).expect("compile");
    let dummy_vm = [0u8; 64];
    let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[1, 2]) }.expect("call");
    assert_eq!(
        r,
        i64::MIN,
        "a throwing direct callee must bail and propagate the sentinel"
    );
}

#[test]
fn ir_direct_call_declines_self_recursion_and_unknown_callees() {
    // Two negative controls for the eligibility gate:
    //  1. A SELF-recursive static call must NOT become a cross-method direct call
    //     (it has its own `invoke_kind == 4` path, whose stack guard is what keeps
    //     runaway recursion a catchable StackOverflowError).
    //  2. A callee the `callee_compiler` cannot compile must keep helper dispatch.
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = positional_dispatch as *const () as usize;
    // static int f(int a, int b) { return f(a, b); }  (self-recursive)
    let code = vec![0x1a, 0x1b, 0xb8, 0x00, 0x02, 0xac];
    let cm = cached("f", "(II)I", code.clone(), 2, 2);
    let self_resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("Corpus".into(), "f".into(), "(II)I".into()))
        } else {
            None
        }
    };
    let entry = direct_callee_ctx as *const () as usize;
    let any_callee =
        |_c: &str, _m: &str, _d: &str| -> Option<(usize, bool)> { Some((entry, true)) };
    let ir = compile_with_direct_callee(&cm, &helpers, &self_resolver, &any_callee)
        .expect("self-recursive compile");
    assert!(
        !ir._direct_callee_entries.contains(&entry),
        "a self-recursive site must never be bound as a CROSS-method direct call"
    );

    // (2) unknown callee → no direct entry recorded, dispatch retained.
    let cm2 = cached("f", "(II)I", code, 2, 2);
    let cross_resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(II)I".into()))
        } else {
            None
        }
    };
    let no_callee = |_c: &str, _m: &str, _d: &str| -> Option<(usize, bool)> { None };
    let ir2 = compile_with_direct_callee(&cm2, &helpers, &cross_resolver, &no_callee)
        .expect("compile with an uncompilable callee");
    assert!(
        ir2._direct_callee_entries.is_empty(),
        "no direct entry may be recorded when the callee cannot be compiled"
    );
    let dummy_vm = [0u8; 64];
    let r = unsafe { ir2.try_call_with_context(dummy_vm.as_ptr() as i64, &[3, 4]) }.expect("call");
    assert_eq!(
        r as i32,
        host_positional(&[3, 4]) as i32,
        "an uncompilable callee must still reach the dispatch helper"
    );
}
