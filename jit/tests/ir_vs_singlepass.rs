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
use cratonvm_types::ClassId;
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
        cm, None, None, None, None, None, None, None, None, None, helpers, None, None, None,
        optimize,
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
        &[
            (vec![-5], 5),
            (vec![0], 0),
            (vec![7], 7),
            (vec![-100], 100),
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
