// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 11, wave 5, lane `x64` — page
//! `r11w3-x64gen-spliced-getfield-pays-region-containment-check`.
//!
//! A `getfield` inside a SPLICED callee used to check its receiver against the
//! whole READ bounds table (null, alignment, three region compares) even when
//! the receiver is the callee's unwritten local 0, bound from an operand the
//! caller's exact type model already marks an oop. It now takes the trusted
//! null test there, as the top-level arm does for the same value.
//!
//! The witness is an EMPTY bounds table: the region check refuses every
//! receiver (so the old code always called `jit_getfield`), while the trusted
//! arm reads the field inline. A callee that overwrites local 0 first is the
//! control: its receiver is not the bound argument, so it keeps the region
//! check and reaches the helper.
//!
//! Harness copied from `r9w3_x64obj3_fields.rs` (receivers are hand-built
//! 8-aligned buffers; every helper is a non-panicking stub).

use cratonvm_jit::JitRuntimeHelpers;
use cratonvm_jit::{try_compile, CachedBytecodeMethod, CompiledMethod, InlineSite};
use cratonvm_types::{ClassId, FIELD_CELL_PAYLOAD32_OFFSET, HEADER_SIZE, NUM_SLOTS_OFFSET, SLOT_SIZE};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A bounds table with no region: `emit_guarded_getfield_receiver_check`
/// sends every receiver to the helper.
static EMPTY: [AtomicUsize; 6] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

const BENIGN: i64 = 777_777;

unsafe extern "C" fn benign() -> i64 {
    BENIGN
}

unsafe extern "C" fn record_throw_bci(_bci: i64) {}

unsafe extern "C" fn self_guard(_vm: i64) -> i64 {
    0
}

unsafe extern "C" fn native_stack_floor() -> i64 {
    0
}

/// Every call target is [`benign`]; READ and STORE bounds are [`EMPTY`].
fn helpers() -> JitRuntimeHelpers {
    let s = benign as *const () as usize;
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
        self_call_stack_guard: self_guard as *const () as usize,
        region_bounds_addr: EMPTY.as_ptr() as usize,
        read_bounds_addr: EMPTY.as_ptr() as usize,
        local_handler_lookup: 0,
        native_stack_floor_fn: native_stack_floor as *const () as usize,
        ldc_string: s,
        set_throw_bci: record_throw_bci as *const () as usize,
        service_callee_deopt: s,
        ..Default::default()
    }
}

/// `code` padded with the VM's two trailing zero bytes, as a static method.
fn cached(
    class: &str,
    name: &str,
    descriptor: &str,
    mut code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
) -> CachedBytecodeMethod {
    code.push(0x00);
    code.push(0x00);
    CachedBytecodeMethod {
        declaring_class_id: ClassId::new(1),
        class_name: Arc::from(class),
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
        pool_generation: u64::MAX,
    }
}

/// An 8-aligned legacy object with one `Value::Int(value)` cell.
fn legacy_int_object(value: i32) -> Vec<u64> {
    let bytes = HEADER_SIZE + SLOT_SIZE;
    let mut buf = vec![0u64; bytes.div_ceil(8)];
    let base = buf.as_mut_ptr() as *mut u8;
    // SAFETY: every offset below is inside the `bytes`-long buffer.
    unsafe {
        std::ptr::write_unaligned(base.add(NUM_SLOTS_OFFSET) as *mut u32, 1);
        let cell = base.add(HEADER_SIZE);
        std::ptr::write_unaligned(cell as *mut u64, 0);
        std::ptr::write_unaligned(cell.add(8) as *mut u64, 0);
        std::ptr::write_unaligned(cell.add(FIELD_CELL_PAYLOAD32_OFFSET) as *mut i32, value);
    }
    buf
}

/// Call a compiled static method, supplying the hidden context slot when the
/// artifact has one (field methods do: their helper edge wants `vm_ptr`).
/// The fast paths under test never dereference it.
fn call(m: &CompiledMethod, args: &[i64]) -> i64 {
    let dummy_vm = [0u64; 16];
    // SAFETY: JIT-compiled from valid bytecode; every reference argument is a
    // live, 8-aligned test buffer (or null), and every reachable helper is a
    // non-panicking stub.
    unsafe {
        if m.needs_context() {
            m.try_call_with_context(dummy_vm.as_ptr() as i64, args)
        } else {
            m.try_call(args)
        }
    }
    .expect("jit call")
}

/// `static int get(Object c)` with `code` as its body (field `v` at slot 0,
/// read by the `getfield` at `getfield_pc`).
fn site(code: Vec<u8>, getfield_pc: usize) -> InlineSite {
    let mut code = code;
    code.push(0x00);
    code.push(0x00);
    InlineSite {
        callee_code_len: code.len() - 2,
        callee_code: code,
        callee_max_locals: 1,
        callee_num_args: 1,
        callee_is_static: true,
        return_type: b'I',
        field_info: vec![(getfield_pc, 0, b'I')],
        needs_heap: true,
        class_name: "r9w3/Cell".to_string(),
        method_name: "get".to_string(),
        descriptor: "(Ljava/lang/Object;)I".to_string(),
        ..InlineSite::default()
    }
}

/// `static int run(Object c) { return Cell.get(c); }` compiled single-pass
/// with `get` handed to the single-pass inliner.
fn compile_caller(site: InlineSite, h: &JitRuntimeHelpers) -> CompiledMethod {
    let cm = cached(
        "r9w3/Caller",
        "run",
        "(Ljava/lang/Object;)I",
        vec![0x2a, 0xb8, 0x00, 0x01, 0xac],
        1,
        1,
    );
    let invoke = |idx: u16| -> Option<(String, String, String)> {
        (idx == 1).then(|| {
            (
                "r9w3/Cell".to_string(),
                "get".to_string(),
                "(Ljava/lang/Object;)I".to_string(),
            )
        })
    };
    let inline = |c: &str, m: &str, _d: &str| -> Option<InlineSite> {
        (c == "r9w3/Cell" && m == "get").then(|| site.clone())
    };
    try_compile(
        &cm,
        None,          // cp_class_name_resolver
        None,          // cp_field_resolver (the caller has no field)
        None,          // cp_static_field_resolver
        Some(&invoke), // cp_invoke_resolver
        None,          // callee_compiler
        None,          // cp_new_resolver
        None,          // cp_ldc_resolver
        None,          // cp_ldc2w_resolver
        None,          // profile
        h,
        Some(&inline), // inline_resolver (single-pass)
        None,          // string_layout_resolver
        None,          // cp_invoke_class_id_resolver
        None,          // cp_elidable_init_resolver
        false,         // optimize: single-pass
        false,
        false,
        false,
        false,
        false,
        None,
    )
    .expect("the caller compiles single-pass")
}

/// `aload_0; getfield; ireturn`: the receiver is the unwritten local 0, bound
/// from the caller's `aload_0` (an oop mark), so the region table is not
/// consulted and the field is read inline even though the table is empty.
/// Before the fix this reached `jit_getfield` (the marker value).
#[test]
fn a_spliced_getfield_of_the_bound_local_0_skips_the_region_table() {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn counting_getfield(_vm: i64, _obj: i64, _idx: i64) -> i64 {
        CALLS.fetch_add(1, Ordering::SeqCst);
        424_242
    }
    let mut h = helpers();
    h.getfield = counting_getfield as *const () as usize;
    let m = compile_caller(site(vec![0x2a, 0xb4, 0x00, 0x01, 0xac], 1), &h);

    let obj = legacy_int_object(42);
    let r = call(&m, &[obj.as_ptr() as i64]);
    assert_ne!(r, BENIGN, "the call to `get` was not spliced");
    assert_eq!(r as i32, 42, "the trusted receiver must be read inline");
    assert_eq!(CALLS.load(Ordering::SeqCst), 0, "no helper call for a trusted receiver");

    // Null is still the helper's to answer.
    let r = call(&m, &[0]);
    assert_eq!(r, 424_242, "a null receiver must reach the helper");
    assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    drop(obj);
}

/// The control: `aload_0; astore_0; aload_0; getfield; ireturn` writes local
/// 0, so its `aload_0` is no longer known to be the bound argument and the
/// region check stays; with the empty table the helper answers.
#[test]
fn a_callee_that_writes_local_0_keeps_the_region_check() {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn counting_getfield(_vm: i64, _obj: i64, _idx: i64) -> i64 {
        CALLS.fetch_add(1, Ordering::SeqCst);
        424_242
    }
    let mut h = helpers();
    h.getfield = counting_getfield as *const () as usize;
    let m = compile_caller(site(vec![0x2a, 0x4b, 0x2a, 0xb4, 0x00, 0x01, 0xac], 3), &h);

    let obj = legacy_int_object(42);
    let r = call(&m, &[obj.as_ptr() as i64]);
    assert_ne!(r, BENIGN, "the call to `get` was not spliced");
    assert_eq!(r, 424_242, "an untrusted receiver meets the (empty) region table");
    assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    drop(obj);
}
