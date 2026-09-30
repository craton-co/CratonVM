// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Round 13 wave 1, lane callcost: the single-pass method compile asks the
//! call-site intrinsic ladder BEFORE it direct-binds a static callee.
//!
//! `try_compile_inner` used to try `plan_inline`, then the eager direct bind
//! (`callee_compiler`), and only then `try_resolve_intrinsic`. A target with no
//! registered native (nothing refuses its bind) therefore got a raw CALL to its
//! compiled JDK body -- for `Math.fma` the `@IntrinsicCandidate` BigDecimal
//! fallback, which is what made `R12Rt3FmaEdges`' `fma-rows` ~1000x HotSpot.
//! `CRATONVM_JIT_STATIC_INTRINSIC_FIRST` (default on) skips both for a site the
//! ladder matches; `0` restores the old order, which this test also pins so
//! the fixture is known to exercise the bind.
//!
//! Metadata only: the bodies are compiled, never run.

use cratonvm_jit::{try_compile_request, CachedBytecodeMethod, CompileRequest};
use cratonvm_jit_api::JitRuntimeHelpers;
use cratonvm_types::ClassId;
use std::cell::RefCell;
use std::sync::Arc;

fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("r13_callcost_static_intrinsic_first invoked a runtime helper");
    }
    let s = stub as *const () as usize;
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
        dispatch_threw: s,
        jit_frem: s,
        jit_drem: s,
        self_call_stack_guard: 0,
        region_bounds_addr: 0,
        native_stack_floor_fn: 0,
        ldc_string: s,
        set_throw_bci: s,
        service_callee_deopt: s,
        ..Default::default()
    }
}

fn static_method(
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
        declaring_class_id: ClassId::new(0x0013_0001),
        class_name: Arc::from(class),
        method_name: Arc::from(name),
        method_descriptor: Arc::from(desc),
        source_file: None,
        code: Arc::from(code.as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 6,
        max_locals,
        num_params: cratonvm_jit::count_param_slots(desc) as u16,
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

/// Compile `double f() { return Math.fma(1.0, 1.0, 0.0); }` with a callee
/// compiler that would bind a (stand-in) compiled `Math.fma` body, under the
/// given value of `CRATONVM_JIT_STATIC_INTRINSIC_FIRST`. Answers the names the
/// callee compiler was asked for and the number of direct callee entries the
/// artifact baked.
fn compile_caller(first: Option<&'static str>) -> (Vec<String>, usize) {
    cratonvm_jit::x64::set_moving_young_override(Some(false));
    let _flags = cratonvm_types::flags::override_thread(
        cratonvm_types::flags::VmFlags::from_env_with_edits(&[
            ("CRATONVM_JIT_DIRECT_CALLEE_CALLS", Some("1")),
            ("CRATONVM_JIT_STATIC_INTRINSIC_FIRST", first),
        ]),
    );
    let helpers = helpers();
    // dconst_1; dconst_1; dconst_0; invokestatic #2; dreturn
    let caller = static_method(
        "pkg/R13Fma",
        "f",
        "()D",
        vec![0x0f, 0x0f, 0x0e, 0xb8, 0x00, 0x02, 0xaf],
        0,
    );
    // The stand-in for the JDK's `Math.fma` body: dconst_0; dreturn.
    let fallback = static_method("java/lang/Math", "fma", "(DDD)D", vec![0x0e, 0xaf], 6);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        (cp == 2).then(|| {
            (
                "java/lang/Math".to_string(),
                "fma".to_string(),
                "(DDD)D".to_string(),
            )
        })
    };
    let asked: RefCell<Vec<String>> = RefCell::new(Vec::new());
    let callee_compiler = |class: &str, name: &str, desc: &str| -> Option<(usize, bool)> {
        asked.borrow_mut().push(format!("{class}.{name}{desc}"));
        let body = try_compile_request(&CompileRequest::new(
            &fallback,
            &helpers,
            cratonvm_jit::CompileRealm::process(),
        ))?;
        let body = Box::leak(Box::new(body));
        Some((body.entry_ptr() as usize, body.needs_context()))
    };
    let mut req = CompileRequest::new(&caller, &helpers, cratonvm_jit::CompileRealm::process());
    req.cp_invoke_resolver = Some(&resolver);
    req.callee_compiler = Some(&callee_compiler);
    let compiled = try_compile_request(&req).expect("the caller compiles");
    let baked = compiled._direct_callee_entries.len();
    let names = asked.borrow().clone();
    (names, baked)
}

#[test]
fn the_ladder_is_asked_before_the_direct_bind() {
    let (asked, baked) = compile_caller(None);
    assert!(
        asked.is_empty(),
        "default: a Math.fma site must not be offered to the callee compiler, got {asked:?}"
    );
    assert_eq!(baked, 0, "default: no direct CALL to the JDK body is baked");
}

#[test]
fn switched_off_the_old_order_binds_the_jdk_body() {
    // Non-vacuity: the fixture does reach the bind when the switch is off
    // (whether the stand-in body then compiles is not this test's question).
    let (asked, _baked) = compile_caller(Some("0"));
    assert_eq!(asked, vec!["java/lang/Math.fma(DDD)D".to_string()]);
}
