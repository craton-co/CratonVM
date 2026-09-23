// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 9, wave 10, lane `call10` -- a statically bound INSTANCE
//! self-call is a direct self-call, not a `jit_invoke_dispatch` round trip
//! (`perf-private-instance-self-recursion-dispatches-every-call-FIXED-20260918.md`).
//!
//! `int s(int d) { return d <= 0 ? 0 : s(d - 1) + 1; }` -- an instance method
//! calling itself through `invokespecial` (what javac < 11 emits for a private
//! method, and what the private `invokevirtual` pin turns javac 11+'s form
//! into) -- is compiled through `try_compile_request` and EXECUTED:
//!
//! * both tiers answer `s(d) == d` with NO dispatch: the site is planned as a
//!   `JIT_SELF_CALL_GUARD` row and lowered by the self-recursive arm (one
//!   stack-guard call per recursion, since this harness wires no stack floor);
//!   the optimizing tier hands the method to single-pass;
//! * a NULL receiver raises NPE AT THE INVOKE (JVMS 6.5): the compiled body
//!   calls `jit_npe_with_action` and returns the sentinel, before any
//!   recursion;
//! * `CRATONVM_JIT_INSTANCE_SELF_CALL=0` restores the dispatch route.

use cratonvm_jit::{try_compile_request, CachedBytecodeMethod, CompileRequest, JitInvokeInfo};
use cratonvm_jit_api::JitRuntimeHelpers;
use cratonvm_types::ClassId;
use std::cell::Cell;
use std::sync::Arc;

const CTX: i64 = 0x1234_5678;
/// A non-null "reference" the compiled bodies below never dereference.
const RECV: i64 = 0x7100_0000_1000;

thread_local! {
    static DISPATCHES: Cell<usize> = const { Cell::new(0) };
    static GUARDS: Cell<usize> = const { Cell::new(0) };
    static NPES: Cell<usize> = const { Cell::new(0) };
}

/// The dispatch fallback: answers the LAST argument (`d - 1`), so a method
/// that dispatches still computes `d`, and counts.
unsafe extern "C" fn counting_dispatch(
    _vm: i64,
    _info: *const JitInvokeInfo,
    args: *const i64,
    n: usize,
) -> i64 {
    DISPATCHES.with(|d| d.set(d.get() + 1));
    assert!(n >= 1);
    // SAFETY: the compiled caller passes `n` live argument slots.
    unsafe { *args.add(n - 1) }
}

unsafe extern "C" fn counting_guard(_vm: i64) -> i64 {
    GUARDS.with(|g| g.set(g.get() + 1));
    0
}

unsafe extern "C" fn counting_npe(_action: i64) {
    NPES.with(|n| n.set(n.get() + 1));
}

fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("r9w10 call10 test invoked an unwired runtime helper");
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
        jit_npe_with_action: counting_npe as *const () as usize,
        dispatch_threw: s,
        jit_frem: s,
        jit_drem: s,
        self_call_stack_guard: counting_guard as *const () as usize,
        region_bounds_addr: 0,
        native_stack_floor_fn: 0,
        ldc_string: s,
        set_throw_bci: record_throw_bci as *const () as usize,
        service_callee_deopt: deopt_unserviceable_stub as *const () as usize,
        ..Default::default()
    }
}

fn instance_method(
    class: &str,
    name: &str,
    desc: &str,
    mut code: Vec<u8>,
    max_locals: u16,
) -> CachedBytecodeMethod {
    // The VM pads every method's bytecode with two zero bytes.
    code.push(0x00);
    code.push(0x00);
    CachedBytecodeMethod {
        declaring_class_id: ClassId::new(0x0010_1001),
        class_name: Arc::from(class),
        method_name: Arc::from(name),
        method_descriptor: Arc::from(desc),
        source_file: None,
        code: Arc::from(code.as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 4,
        max_locals,
        num_params: cratonvm_jit::count_param_slots(desc) as u16,
        is_synchronized: false,
        is_static: false,
        force_native_cache: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    }
}

/// `int s(int d) { return d <= 0 ? 0 : s(d - 1) + 1; }`, `s` an instance
/// method of `R9W10Self` called through `invokespecial #2`.
fn self_method() -> CachedBytecodeMethod {
    let code = vec![
        0x1b, // 0: iload_1
        0x9d, 0x00, 0x05, // 1: ifgt +5 (6)
        0x03, // 4: iconst_0
        0xac, // 5: ireturn
        0x2a, // 6: aload_0
        0x1b, // 7: iload_1
        0x04, // 8: iconst_1
        0x64, // 9: isub
        0xb7, 0x00, 0x02, // 10: invokespecial #2 (this method)
        0x04, // 13: iconst_1
        0x60, // 14: iadd
        0xac, // 15: ireturn
    ];
    instance_method("R9W10Self", "s", "(I)I", code, 2)
}

/// `int t(R9W10Self o, int d) { return d <= 0 ? 0 : o.t(o, d - 1) + 1; }` --
/// the receiver is a PARAMETER, so it can be null.
fn param_receiver_method() -> CachedBytecodeMethod {
    let code = vec![
        0x1c, // 0: iload_2
        0x9d, 0x00, 0x05, // 1: ifgt +5 (6)
        0x03, // 4: iconst_0
        0xac, // 5: ireturn
        0x2b, // 6: aload_1
        0x2b, // 7: aload_1
        0x1c, // 8: iload_2
        0x04, // 9: iconst_1
        0x64, // 10: isub
        0xb7, 0x00, 0x02, // 11: invokespecial #2 (this method)
        0x04, // 14: iconst_1
        0x60, // 15: iadd
        0xac, // 16: ireturn
    ];
    instance_method("R9W10Self", "t", "(LR9W10Self;I)I", code, 3)
}

fn compile(cm: &CachedBytecodeMethod, optimize: bool) -> cratonvm_jit::CompiledMethod {
    cratonvm_jit::ir_evidence::force_accept_always_for_this_process();
    cratonvm_jit::x64::set_moving_young_override(Some(false));
    let helpers = helpers();
    let class = cm.class_name.to_string();
    let name = cm.method_name.to_string();
    let desc = cm.method_descriptor.to_string();
    let resolver = move |cp: u16| -> Option<(String, String, String)> {
        (cp == 2).then(|| (class.clone(), name.clone(), desc.clone()))
    };
    let mut req = CompileRequest::new(cm, &helpers);
    req.cp_invoke_resolver = Some(&resolver);
    req.self_call_identity_stable = true;
    req.optimize = optimize;
    req.ir_emit_calls = true;
    req.ir_emit_special_calls = true;
    req.ir_emit_virtual_calls = true;
    try_compile_request(&req).unwrap_or_else(|| {
        panic!(
            "{}.{} (optimize={optimize}) failed to compile",
            cm.class_name, cm.method_name
        )
    })
}

/// Run; answer (result, dispatches, guard calls, NPEs raised).
fn run(m: &cratonvm_jit::CompiledMethod, args: &[i64]) -> (i64, usize, usize, usize) {
    let (d0, g0, n0) = (
        DISPATCHES.with(Cell::get),
        GUARDS.with(Cell::get),
        NPES.with(Cell::get),
    );
    // SAFETY: JIT code compiled by this test from valid bytecode; everything
    // it can call is a stub above or its own entry.
    let got = unsafe { m.try_call_with_context(CTX, args) }.expect("test JIT call");
    (
        got,
        DISPATCHES.with(Cell::get) - d0,
        GUARDS.with(Cell::get) - g0,
        NPES.with(Cell::get) - n0,
    )
}

#[test]
fn an_instance_self_call_recurses_directly_in_both_tiers() {
    let cm = self_method();
    for optimize in [false, true] {
        let m = compile(&cm, optimize);
        assert!(
            !m.used_ir_backend,
            "optimize={optimize}: the method is handed to the single-pass self-call route"
        );
        for d in [0i64, 1, 5, 40] {
            let (got, dispatches, guards, npes) = run(&m, &[RECV, d]);
            assert_eq!(got as i32, d as i32, "optimize={optimize}: s({d})");
            assert_eq!(
                dispatches, 0,
                "optimize={optimize}: s({d}) must not dispatch"
            );
            assert_eq!(guards, d as usize, "one stack-guard check per recursion");
            assert_eq!(npes, 0);
        }
    }
}

#[test]
fn a_null_receiver_raises_npe_at_the_invoke() {
    let m = compile(&param_receiver_method(), false);
    // Non-null: recurses directly.
    let (got, dispatches, guards, npes) = run(&m, &[RECV, RECV, 3]);
    assert_eq!((got as i32, dispatches, guards, npes), (3, 0, 3, 0));
    // d = 0 never reaches the invoke, so a null `o` is fine.
    assert_eq!(run(&m, &[RECV, 0, 0]), (0, 0, 0, 0));
    // A null receiver at the invoke: NPE, before any recursion.
    let (got, dispatches, guards, npes) = run(&m, &[RECV, 0, 2]);
    assert_eq!(got, i64::MIN, "the NPE path returns the exception sentinel");
    assert_eq!((dispatches, guards, npes), (0, 0, 1));
}

#[test]
fn the_switch_off_restores_the_dispatch_route() {
    let _off = cratonvm_types::flags::override_thread(
        cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
            "CRATONVM_JIT_INSTANCE_SELF_CALL",
            Some("0"),
        )]),
    );
    let m = compile(&self_method(), false);
    let (got, dispatches, guards, _) = run(&m, &[RECV, 4]);
    assert_eq!(got as i32, 4);
    assert_eq!(
        dispatches, 1,
        "the top call dispatches once (the stub answers d - 1)"
    );
    assert_eq!(guards, 0);
}
