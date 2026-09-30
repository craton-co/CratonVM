// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Round 12 wave 2, lane irbuild: the sparse-switch SEARCH TREE the optimizing
//! front end builds for a strictly ascending `lookupswitch` / `tableswitch`
//! with at least eight live keys that the dense `Op::Switch` form refuses
//! (`ir::switch_search_tree_cases`, `IrBuilder::build_switch_search_tree`).
//!
//! Executed, not only inspected: the same method is compiled through the IR
//! pipeline and through the single-pass backend, and both must return the
//! host-computed answer for every key, every gap next to a key, and both ends
//! of the `int` range. A wrong pivot split or a wrong signed/unsigned compare
//! shows up as a divergence here.
//!
//! The harness is a trimmed copy of `jit/tests/ir_vs_singlepass.rs`'s
//! (pure-int methods reach no runtime helper).

use cratonvm_jit::{try_compile, CachedBytecodeMethod, CompiledMethod, JitRuntimeHelpers};
use cratonvm_types::ClassId;
use std::sync::Arc;

static TEST_REGION_BOUNDS: [std::sync::atomic::AtomicUsize; 6] = [
    std::sync::atomic::AtomicUsize::new(0x1000),
    std::sync::atomic::AtomicUsize::new(usize::MAX),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
];

fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("r12_irbuild_switch_tree invoked an unwired runtime helper");
    }
    unsafe extern "C" fn self_guard(_vm_ptr: i64) -> i64 {
        0
    }
    unsafe extern "C" fn native_stack_floor() -> i64 {
        0
    }
    unsafe extern "C" fn record_throw_bci(_bci: i64) {}
    unsafe extern "C" fn deopt_unserviceable_stub(
        _vm: i64,
        _info: i64,
        _args: i64,
        _n: i64,
    ) -> i64 {
        i64::MIN
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
        self_call_stack_guard: self_guard as *const () as usize,
        region_bounds_addr: TEST_REGION_BOUNDS.as_ptr() as usize,
        read_bounds_addr: TEST_REGION_BOUNDS.as_ptr() as usize,
        local_handler_lookup: 0,
        native_stack_floor_fn: native_stack_floor as *const () as usize,
        ldc_string: s,
        set_throw_bci: record_throw_bci as *const () as usize,
        service_callee_deopt: deopt_unserviceable_stub as *const () as usize,
        ..Default::default()
    }
}

fn cached(name: &str, mut code: Vec<u8>) -> CachedBytecodeMethod {
    // The VM pads every method with two trailing zero bytes; `try_compile`
    // recovers the real length as `code.len() - 2`.
    code.push(0x00);
    code.push(0x00);
    CachedBytecodeMethod {
        declaring_class_id: ClassId::new(1),
        class_name: Arc::from("Corpus"),
        method_name: Arc::from(name),
        method_descriptor: Arc::from("(I)I"),
        source_file: None,
        code: Arc::from(code.as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 8,
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

fn compile(cm: &CachedBytecodeMethod, h: &JitRuntimeHelpers, optimize: bool) -> Option<CompiledMethod> {
    // Routing, not policy: without this the C1->C2 acceptance gate refuses
    // the IR body and the comparison would be single-pass against itself.
    cratonvm_jit::ir_evidence::force_accept_always_for_this_process();
    try_compile(
        cm, None, None, None, None, None, None, None, None, None, h, None, None, None, None,
        optimize, false, false, false, false, false, None,
    )
}

/// `static int f(int k) { switch (k) { case keys[i]: return values[i]; …
/// default: return dflt; } }` as a `lookupswitch`.
fn lookupswitch_method(pairs: &[(i32, i16)], dflt: i16) -> Vec<u8> {
    let n = pairs.len();
    let arm_base = 12 + 8 * n;
    let default_at = arm_base + 4 * n;
    let mut code = vec![0x1a, 0xab, 0x00, 0x00];
    code.extend_from_slice(&((default_at as i32) - 1).to_be_bytes());
    code.extend_from_slice(&(n as i32).to_be_bytes());
    for (i, &(key, _)) in pairs.iter().enumerate() {
        code.extend_from_slice(&key.to_be_bytes());
        code.extend_from_slice(&(((arm_base + 4 * i) as i32) - 1).to_be_bytes());
    }
    for &(_, value) in pairs {
        code.push(0x11); // sipush
        code.extend_from_slice(&value.to_be_bytes());
        code.push(0xac); // ireturn
    }
    code.push(0x11);
    code.extend_from_slice(&dflt.to_be_bytes());
    code.push(0xac);
    code
}

fn check(name: &str, pairs: &[(i32, i16)], dflt: i16) {
    let h = helpers();
    let cm = cached(name, lookupswitch_method(pairs, dflt));
    let ir = compile(&cm, &h, true).unwrap_or_else(|| panic!("{name}: IR compile failed"));
    let sp = compile(&cm, &h, false).unwrap_or_else(|| panic!("{name}: single-pass compile failed"));
    let mut keys: Vec<i32> = vec![i32::MIN, i32::MIN + 1, -1, 0, 1, i32::MAX - 1, i32::MAX];
    for &(k, _) in pairs {
        keys.push(k);
        keys.push(k.wrapping_sub(1));
        keys.push(k.wrapping_add(1));
    }
    for k in keys {
        let want = pairs
            .iter()
            .find(|&&(key, _)| key == k)
            .map_or(i32::from(dflt), |&(_, v)| i32::from(v));
        let args = vec![i64::from(k)];
        // SAFETY: both bodies were compiled from valid pure-int bytecode; the
        // System V i64-arg / i64-ret entry ABI is the one `try_call` uses, and
        // no runtime helper is reachable from this method.
        let r_ir = unsafe { ir.try_call(&args) }.unwrap_or_else(|e| panic!("{name}: IR {k}: {e:?}"));
        let r_sp =
            unsafe { sp.try_call(&args) }.unwrap_or_else(|e| panic!("{name}: single-pass {k}: {e:?}"));
        assert_eq!(r_ir as i32, r_sp as i32, "{name}: IR vs single-pass diverge for key {k}");
        assert_eq!(r_ir as i32, want, "{name}: wrong arm for key {k}");
    }
}

/// Forty keys scattered like `String.hashCode()` values, negative ones
/// included: far too sparse for `Op::Switch`, so the builder takes the tree.
#[test]
fn a_sparse_forty_key_lookupswitch_dispatches_like_the_single_pass_tier() {
    let mut pairs: Vec<(i32, i16)> = (0..40i32)
        .map(|i| {
            let key = i.wrapping_mul(0x9E37_79B9_u32 as i32) ^ (i << 7);
            (key, (i * 3 + 1) as i16)
        })
        .collect();
    pairs.sort_unstable_by_key(|&(k, _)| k);
    pairs.dedup_by_key(|p| p.0);
    assert!(pairs.len() >= 8);
    check("sparse40", &pairs, -9);
}

/// Exactly the threshold, with both ends of the `int` range as keys: the
/// signed pivot compare must order `i32::MIN` below everything.
#[test]
fn an_eight_key_lookupswitch_at_the_int_extremes() {
    let pairs = [
        (i32::MIN, 1),
        (-70_000, 2),
        (-3, 3),
        (0, 4),
        (5, 5),
        (65_536, 6),
        (1 << 29, 7),
        (i32::MAX, 8),
    ];
    check("extremes8", &pairs, 0);
}
