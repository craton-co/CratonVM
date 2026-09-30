// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 11, wave 11, lane `calls` -- bounded self-recursive
//! inlining of a MULTI-RETURN callee (`CratonBench fib`).
//!
//! `static int fib(int n) { if (n <= 1) return n; return fib(n-1) + fib(n-2); }`
//! has two `return`s, which the splice scanner refused
//! (`ir-splice-not-single-trailing-return`), so every one of `fib(44)`'s
//! ~2.3e9 calls was a real direct self-call. With `ir_recursive_inline_enabled`
//! (default on since this wave) the optimizing tier splices two levels of
//! `fib` into itself at each of its two call sites (2 + 4 = 6 multi-return
//! bodies) and binds the eight calls at the cut to its own entry
//! (`invoke_kind == 4`).
//!
//! The method is compiled through `try_compile_request` with an IR inline
//! resolver that hands back `fib`'s own body -- the tree
//! `resolve_inline_site_from` builds for it -- and EXECUTED against the host
//! value, in both arms of the switch. One `#[test]`, on purpose: the
//! multi-return splice census it reads is process-wide, and this binary runs
//! nothing else, so the "off" arm can assert that nothing was spliced.

use cratonvm_jit::{
    try_compile_request, CachedBytecodeMethod, CompileRequest, InlineInvokeTarget, InlineSite,
    JitInvokeInfo, NestedInlineSite,
};
use cratonvm_jit_api::JitRuntimeHelpers;
use cratonvm_types::ClassId;
use std::cell::Cell;
use std::sync::Arc;

const CLASS_ID: u32 = 0x0010_1001;
const CLASS: &str = "R11CallsFib";

thread_local! {
    static DISPATCHES: Cell<usize> = const { Cell::new(0) };
}

unsafe extern "C" fn counting_dispatch(
    _vm: i64,
    _info: *const JitInvokeInfo,
    _args: *const i64,
    _n: usize,
) -> i64 {
    DISPATCHES.with(|d| d.set(d.get() + 1));
    // Never a correct answer: a dispatched self-call fails the value check.
    -1
}

unsafe extern "C" fn quiet_guard(_vm: i64) -> i64 {
    0
}

fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("r11 calls recursive-inline test invoked an unwired runtime helper");
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
        invoke_dispatch: counting_dispatch as *const () as usize,
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
        self_call_stack_guard: quiet_guard as *const () as usize,
        region_bounds_addr: 0,
        native_stack_floor_fn: 0,
        ldc_string: s,
        set_throw_bci: record_throw_bci as *const () as usize,
        service_callee_deopt: deopt_unserviceable_stub as *const () as usize,
        ..Default::default()
    }
}

/// `fib`'s bytecode (21 bytes): the base case returns at pc 6, the self-calls
/// are at pc 10 and pc 16, the recursive case returns at pc 20.
fn fib_code() -> Vec<u8> {
    vec![
        0x1a, // 0: iload_0
        0x04, // 1: iconst_1
        0xa3, 0x00, 0x05, // 2: if_icmpgt +5 (7)
        0x1a, // 5: iload_0
        0xac, // 6: ireturn
        0x1a, // 7: iload_0
        0x04, // 8: iconst_1
        0x64, // 9: isub
        0xb8, 0x00, 0x02, // 10: invokestatic #2 (fib)
        0x1a, // 13: iload_0
        0x05, // 14: iconst_2
        0x64, // 15: isub
        0xb8, 0x00, 0x02, // 16: invokestatic #2 (fib)
        0x60, // 19: iadd
        0xac, // 20: ireturn
    ]
}

fn fib_method() -> CachedBytecodeMethod {
    let mut code = fib_code();
    // The VM pads every method's bytecode with two zero bytes.
    code.push(0x00);
    code.push(0x00);
    CachedBytecodeMethod {
        declaring_class_id: ClassId::new(CLASS_ID),
        class_name: Arc::from(CLASS),
        method_name: Arc::from("fib"),
        method_descriptor: Arc::from("(I)I"),
        source_file: None,
        code: Arc::from(code.as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 4,
        max_locals: 1,
        num_params: 1,
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
        pool_generation: u64::MAX,
    }
}

fn self_target() -> InlineInvokeTarget {
    InlineInvokeTarget {
        class_name: CLASS.to_string(),
        method_name: "fib".to_string(),
        descriptor: "(I)I".to_string(),
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 3,
        declaring_class_id: CLASS_ID,
        ..Default::default()
    }
}

/// `fib`'s body as the IR resolver returns it, with its two self-calls nested
/// `depth` more levels -- and, like the resolver's output on a first compile,
/// no `direct_entry` on any of them (the callee compiler declines the cycle).
fn fib_site(depth: usize) -> InlineSite {
    let mut callee_code = fib_code();
    callee_code.push(0x00);
    callee_code.push(0x00);
    let nested_sites = if depth == 0 {
        Vec::new()
    } else {
        [10usize, 16]
            .into_iter()
            .map(|pc| NestedInlineSite {
                callee_pc: pc,
                guard_class_id: 0,
                site: fib_site(depth - 1),
            })
            .collect()
    };
    InlineSite {
        callee_code,
        callee_code_len: 21,
        callee_max_locals: 1,
        callee_num_args: 1,
        callee_is_static: true,
        return_type: b'I',
        needs_heap: true,
        class_name: CLASS.to_string(),
        class_id: CLASS_ID,
        method_name: "fib".to_string(),
        descriptor: "(I)I".to_string(),
        invoke_targets: vec![(10, self_target()), (16, self_target())],
        nested_sites,
        ..Default::default()
    }
}

fn compile(cm: &CachedBytecodeMethod) -> cratonvm_jit::CompiledMethod {
    cratonvm_jit::ir_evidence::force_accept_always_for_this_process();
    cratonvm_jit::x64::set_moving_young_override(Some(false));
    let helpers = helpers();
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        (cp == 2).then(|| (CLASS.to_string(), "fib".to_string(), "(I)I".to_string()))
    };
    let ir_inline_resolver = |class: &str, name: &str, desc: &str| -> Option<InlineSite> {
        (class == CLASS && name == "fib" && desc == "(I)I").then(|| fib_site(3))
    };
    let mut req = CompileRequest::new(cm, &helpers, cratonvm_jit::CompileRealm::process());
    req.cp_invoke_resolver = Some(&resolver);
    req.ir_inline_resolver = Some(&ir_inline_resolver);
    req.self_call_identity_stable = true;
    req.optimize = true;
    req.ir_emit_calls = true;
    try_compile_request(&req).expect("fib compiles")
}

fn host_fib(n: i64) -> i64 {
    if n <= 1 {
        n
    } else {
        host_fib(n - 1) + host_fib(n - 2)
    }
}

fn run_all(m: &cratonvm_jit::CompiledMethod, arm: &str) {
    let dummy_vm = [0u8; 64];
    let d0 = DISPATCHES.with(Cell::get);
    for n in [-3i64, 0, 1, 2, 3, 5, 10, 20, 25] {
        // SAFETY: JIT code compiled by this test from valid bytecode; it calls
        // only its own entry and the stubs above, and never dereferences the
        // context pointer.
        let got = unsafe { m.try_call_with_context(dummy_vm.as_ptr() as i64, &[n]) }
            .unwrap_or_else(|e| panic!("{arm}: fib({n}): {e:?}"));
        assert_eq!(got as i32, host_fib(n) as i32, "{arm}: fib({n})");
    }
    assert_eq!(
        DISPATCHES.with(Cell::get) - d0,
        0,
        "{arm}: no call may dispatch"
    );
}

// Round 11 wave 12 (lane fib): un-ignored. The lowerer refused the spliced
// body (`unallocated_value`) because `phi_home_droppable` dropped the home of a
// multi-return result phi that had no register; see
// docs/internal/fixed-bugs/r11w11-orch-recursive-inline-bodies-refused-unallocated-phi-FIXED-20260924.md.
#[test]
fn a_multi_return_self_recursive_method_is_spliced_into_itself_two_levels_deep() {
    let cm = fib_method();

    // Off: the multi-return body is not spliced anywhere (no plan, and the
    // general multi-return switch is off), and `fib` is the plain direct
    // self-call it always was.
    let (bodies0, _) = cratonvm_jit::ir::multi_return_splice_census();
    let off = cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_JIT_IR_RECURSIVE_INLINE", Some("0"))],
        || compile(&cm),
    );
    let (bodies1, _) = cratonvm_jit::ir::multi_return_splice_census();
    assert!(off.used_ir_backend, "off: fib stays on the optimizing tier");
    assert_eq!(bodies1, bodies0, "off: no multi-return body is spliced");
    run_all(&off, "off");

    // On (`=1`): six multi-return copies -- two at each of the two
    // call sites, each with its own two -- and the eight calls at the cut
    // recurse into THIS body.
    let on = cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_JIT_IR_RECURSIVE_INLINE", Some("1"))],
        || compile(&cm),
    );
    let (bodies2, edges2) = cratonvm_jit::ir::multi_return_splice_census();
    assert!(on.used_ir_backend, "on: fib stays on the optimizing tier");
    // At least: a compile that builds the graph more than once (a retry)
    // counts each build.
    assert!(
        bodies2 - bodies1 >= 6,
        "on: 2 + 4 spliced multi-return bodies, got {}",
        bodies2 - bodies1
    );
    assert!(edges2 >= 12, "on: two return edges per body");
    assert!(
        on.code_bytes().len() > off.code_bytes().len(),
        "on: the spliced copies are in the body"
    );
    run_all(&on, "on");
}
