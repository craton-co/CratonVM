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
use cratonvm_types::{ClassId, FIELD_CELL_PAYLOAD32_OFFSET, HEADER_SIZE, SLOT_SIZE};
use std::sync::Arc;

/// Dummy runtime helpers — the corpus is pure arithmetic / branches / counted
/// loops, so no helper (alloc, field, dispatch) is ever invoked; the stub
/// pointer is baked but never called.
fn dummy_helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("ir_vs_singlepass invoked an unwired runtime helper");
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
        invoke_dispatch: s,
        invoke_virtual_mic: s,
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
    }
}

fn compile_opt(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    optimize: bool,
) -> Option<CompiledMethod> {
    try_compile(
        cm, None, None, None, None, None, None, None, None, None, helpers, None, None, None, None,
        optimize, false, false, false, false,
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
        optimize, false, false, true, false,
    )
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
            (vec![3, 4], 8),                              // 12 - 4
            (vec![0x1_0000_0000, 3], 0x3_0000_0000 - 3),  // 64-bit mul
            (vec![-2, 5], -15),                           // -10 - 5
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
        assert_eq!(r_ir as i32, r_sp as i32, "l2i IR vs single-pass for ({a},{b})");
        assert_eq!(
            r_ir as i32,
            a.wrapping_add(b) as i32,
            "l2i vs host for ({a},{b})"
        );
    }
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
fn make_object(fields: &[i32]) -> Vec<u8> {
    let mut buf = vec![0u8; HEADER_SIZE + fields.len() * SLOT_SIZE];
    for (i, &v) in fields.iter().enumerate() {
        let off = HEADER_SIZE + i * SLOT_SIZE + FIELD_CELL_PAYLOAD32_OFFSET;
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
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
    try_compile(
        cm,
        None,
        Some(field_resolver),
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
    for (fields, expected) in cases {
        let obj = make_object(fields);
        let args = [obj.as_ptr() as i64];
        // SAFETY: both bodies were JIT-compiled from valid getfield bytecode;
        // `obj` is a live, correctly-laid-out heap object whose address is the
        // sole (reference) argument, and it outlives both calls (dropped at the
        // end of this iteration). No runtime helper is reachable (inline
        // getfield emits no call).
        let r_sp = unsafe { sp.try_call(&args) }
            .unwrap_or_else(|e| panic!("{name}: single-pass call {fields:?}: {e:?}"));
        let r_ir = unsafe { ir.try_call(&args) }
            .unwrap_or_else(|e| panic!("{name}: IR call {fields:?}: {e:?}"));
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
fn read_field(buf: &[u8], i: usize) -> i32 {
    let off = HEADER_SIZE + i * SLOT_SIZE + FIELD_CELL_PAYLOAD32_OFFSET;
    i32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
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
        true, // optimize (C2 / IR pipeline)
        true, // ir_emit_calls (Gap B, invokestatic)
        true, // ir_emit_special_calls (inc 24, invokespecial — inert without 0xb7)
        false, // ir_emit_long (inc 25 — call tests don't use long)
        true, // ir_emit_virtual_calls (inc 26, invokevirtual/interface)
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
    assert!(ir.needs_context(), "an Op::Call method must report needs_context");
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
    assert!(ir.needs_context(), "an Op::Call method must report needs_context");
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
