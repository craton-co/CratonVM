// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Interpreter round i1, wave 14, lane L2 -- a site whose owner the compile
//! SUBSTITUTED for the constant-pool class (`cp_invokespecial_owner_resolver`:
//! the declaring class of a private or `final` target, a super call's JVMS
//! §6.5 selection start) carries that owner's class id to run time in
//! `JitInvokeInfo::owner_class_id`, so `jit_invoke_dispatch` need not resolve
//! the substituted NAME through the caller's loader
//! (`interpreter-L2-substituted-owner-dispatch-resolves-the-declaring-class-by-name`).
//!
//! Each method below is compiled through `try_compile_request` with no direct
//! binder, so its one call site stays on the dispatch helper, and EXECUTED:
//! the stub helper records the `JitInvokeInfo` it is handed.

use cratonvm_jit::{try_compile_request, CachedBytecodeMethod, CompileRequest, JitInvokeInfo};
use cratonvm_jit_api::JitRuntimeHelpers;
use cratonvm_types::ClassId;
use std::cell::RefCell;
use std::sync::Arc;

const CTX: i64 = 0x1234_5678;
/// A non-null "reference" the compiled bodies below never dereference.
const RECV: i64 = 0x7100_0000_1000;
/// The class every method below is declared in.
const HOLDER: u32 = 0x0010_1401;

thread_local! {
    /// `(class_name, invoke_kind, owner_class_id)` of every dispatched site.
    static SEEN: RefCell<Vec<(String, u8, u32)>> = const { RefCell::new(Vec::new()) };
}

unsafe extern "C" fn recording_dispatch(
    _vm: i64,
    info: *const JitInvokeInfo,
    _args: *const i64,
    _n: usize,
) -> i64 {
    // SAFETY: the compiled caller passes the address of a `JitInvokeInfo` the
    // artifact owns.
    let info = unsafe { &*info };
    let seen = (
        info.class_name.to_string(),
        info.invoke_kind,
        info.owner_class_id,
    );
    SEEN.with(|s| s.borrow_mut().push(seen));
    41
}

fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("i1w14 L2 test invoked an unwired runtime helper");
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
    unsafe extern "C" fn npe(_action: i64) {}
    unsafe extern "C" fn guard(_vm: i64) -> i64 {
        0
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
        invoke_dispatch: recording_dispatch as *const () as usize,
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
        jit_npe_with_action: npe as *const () as usize,
        dispatch_threw: s,
        jit_frem: s,
        jit_drem: s,
        self_call_stack_guard: guard as *const () as usize,
        region_bounds_addr: 0,
        native_stack_floor_fn: 0,
        ldc_string: s,
        set_throw_bci: record_throw_bci as *const () as usize,
        service_callee_deopt: deopt_unserviceable_stub as *const () as usize,
        ..Default::default()
    }
}

fn instance_method(
    name: &str,
    desc: &str,
    mut code: Vec<u8>,
    max_locals: u16,
) -> CachedBytecodeMethod {
    // The VM pads every method's bytecode with two zero bytes.
    code.push(0x00);
    code.push(0x00);
    CachedBytecodeMethod {
        declaring_class_id: ClassId::new(HOLDER),
        class_name: Arc::from("pkg/I1w14Caller"),
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

/// Compile `cm`, whose only invoke is at cp index 2 and names
/// `cp_class.length()I`; the owner resolver answers `owner` for that site
/// when asked with `opcode`.
fn compile(
    cm: &CachedBytecodeMethod,
    optimize: bool,
    cp_class: &'static str,
    opcode: u8,
    owner: (&'static str, u32),
) -> cratonvm_jit::CompiledMethod {
    cratonvm_jit::ir_evidence::force_accept_always_for_this_process();
    cratonvm_jit::x64::set_moving_young_override(Some(false));
    let helpers = helpers();
    let resolver = move |cp: u16| -> Option<(String, String, String)> {
        (cp == 2).then(|| {
            (
                cp_class.to_string(),
                "length".to_string(),
                "()I".to_string(),
            )
        })
    };
    let owner_resolver = move |cp: u16, op: u8| -> Option<(String, u32)> {
        (cp == 2 && op == opcode).then(|| (owner.0.to_string(), owner.1))
    };
    let mut req = CompileRequest::new(cm, &helpers, cratonvm_jit::CompileRealm::process());
    req.cp_invoke_resolver = Some(&resolver);
    req.cp_invokespecial_owner_resolver = Some(&owner_resolver);
    req.optimize = optimize;
    req.ir_emit_calls = true;
    req.ir_emit_special_calls = true;
    req.ir_emit_virtual_calls = true;
    try_compile_request(&req)
        .unwrap_or_else(|| panic!("{} (optimize={optimize}) failed to compile", cm.method_name))
}

/// Run `m` on `args` and answer what the dispatch helper saw.
fn dispatched(m: &cratonvm_jit::CompiledMethod, args: &[i64]) -> Vec<(String, u8, u32)> {
    SEEN.with(|s| s.borrow_mut().clear());
    // SAFETY: JIT code compiled by this test from valid bytecode; the only
    // helper it reaches is the recording stub above.
    let got = unsafe { m.try_call_with_context(CTX, args) }.expect("test JIT call");
    assert_eq!(got as i32, 41, "the stub's answer comes back");
    SEEN.with(|s| s.borrow().clone())
}

/// `int f(pkg/FinalSub o) { return o.length(); }` -- `invokevirtual #2` on a
/// `final` class whose `length()` a package-private ancestor declares: the
/// `StringBuilder.length()` shape.
#[test]
fn a_final_owner_substitution_bakes_the_owner_id() {
    let code = vec![
        0x2b, // 0: aload_1
        0xb6, 0x00, 0x02, // 1: invokevirtual #2
        0xac, // 4: ireturn
    ];
    let cm = instance_method("f", "(Lpkg/FinalSub;)I", code, 2);
    let m = compile(&cm, false, "pkg/FinalSub", 0xb6, ("pkg/Ancestor", 4242));
    // The info the artifact owns names the substituted owner WITH its id.
    let info = m
        ._jit_invoke_infos
        .iter()
        .find(|i| i.method_name == "length")
        .expect("the site has a dispatch row");
    assert_eq!(
        (
            info.class_name,
            info.invoke_kind,
            info.owner_class_id,
            info.declaring_class_id
        ),
        ("pkg/Ancestor", 1, 4242, HOLDER)
    );
    // And it is the one the compiled code hands the helper.
    assert_eq!(
        dispatched(&m, &[RECV, RECV]),
        vec![("pkg/Ancestor".to_string(), 1, 4242)]
    );
}

/// `int g() { return super.length(); }` whose constant pool names a
/// grandparent: the §6.5 selection start and its id are baked, in both tiers
/// (the optimizing tier used to bake the constant-pool class for the
/// dispatch row while binding the selection start).
#[test]
fn a_super_call_redirect_bakes_the_selection_start_and_its_id_in_both_tiers() {
    let code = vec![
        0x2a, // 0: aload_0
        0xb7, 0x00, 0x02, // 1: invokespecial #2
        0xac, // 4: ireturn
    ];
    let cm = instance_method("g", "()I", code, 1);
    for optimize in [false, true] {
        let m = compile(&cm, optimize, "pkg/Grandparent", 0xb7, ("pkg/Parent", 777));
        assert_eq!(
            dispatched(&m, &[RECV]),
            vec![("pkg/Parent".to_string(), 1, 777)],
            "optimize={optimize} (IR backend used: {})",
            m.used_ir_backend
        );
    }
}

/// No substitution: the constant pool's own name, and `0`.
#[test]
fn an_unsubstituted_site_carries_no_owner_id() {
    let code = vec![
        0x2a, // 0: aload_0
        0xb7, 0x00, 0x02, // 1: invokespecial #2
        0xac, // 4: ireturn
    ];
    let cm = instance_method("h", "()I", code, 1);
    // The resolver answers only for `invokevirtual`, never for this site.
    let m = compile(&cm, false, "pkg/Parent", 0xb6, ("pkg/Elsewhere", 9));
    assert_eq!(
        dispatched(&m, &[RECV]),
        vec![("pkg/Parent".to_string(), 1, 0)]
    );
}
