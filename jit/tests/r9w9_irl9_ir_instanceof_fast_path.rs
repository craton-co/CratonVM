// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 9, wave 9, lane `irl9` — the optimizing tier's inline
//! `instanceof` (`perf-instanceof-final-user-class-fast-path-misses-in-a-mixed-main`).
//!
//! Until this wave `ir_lower`'s `Op::InstanceOf` arm was an unconditional
//! `jit_instanceof` CALL: no null answer, no exact-class hit, no final miss.
//! The single-pass body had all three, so the same `instOf` loop ran 15x
//! slower once its C2 body superseded the C1 one. This executes the IR body
//! (asserted: `used_ir_backend`) against header-only fake objects and counts
//! the helper calls, so "answered inline" is measured rather than inferred:
//!
//! * a final BOOT-class target (`java/lang/String`): null, exact hit and a
//!   plain-object miss are inline; a receiver id >= 0x8000_0000 and an array
//!   header still ask the helper;
//! * a non-final user-class target: null and the exact hit are inline, a miss
//!   asks the helper;
//! * a 1-D primitive-array target (`[I`): the matching array tag and a plain
//!   object are inline, another array kind asks the helper.
//!
//! One `#[test]` in its own binary: the helper counter is process-wide, and the
//! duplicate-class-name latch (which routes every miss to the helper) is down
//! unless something in this binary raised it.

use cratonvm_jit::{
    try_compile_request, CachedBytecodeMethod, CompileRequest, CompiledMethod, JitNewSite,
    JitRuntimeHelpers,
};
use cratonvm_types::ClassId;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

static HITS: AtomicUsize = AtomicUsize::new(0);

/// Wide-open region bounds, as `ir_vs_singlepass.rs` provides them.
static TEST_REGION_BOUNDS: [AtomicUsize; 6] = [
    AtomicUsize::new(0x1000),
    AtomicUsize::new(usize::MAX),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

/// Marker `jit_instanceof`: counts, and answers `1` — an answer the inline
/// `0` stub can never produce, so a helper-served miss is visible twice.
unsafe extern "C" fn marker_instanceof(_vm: i64, _obj: i64, _name: i64, _len: i64) -> i64 {
    HITS.fetch_add(1, Ordering::SeqCst);
    1
}

fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("r9w9 irl9 test invoked an unwired runtime helper");
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
        instanceof_check: marker_instanceof as *const () as usize,
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

/// `static boolean f(Object o) { return o instanceof <cp 1>; }`, padded the
/// way the VM pads bytecode (two trailing zeros).
fn instanceof_method(name: &str) -> CachedBytecodeMethod {
    let code: Vec<u8> = vec![0x2a, 0xc1, 0x00, 0x01, 0xac, 0x00, 0x00];
    CachedBytecodeMethod {
        declaring_class_id: ClassId::new(1),
        class_name: Arc::from("R9w9Irl9"),
        method_name: Arc::from(name),
        method_descriptor: Arc::from("(Ljava/lang/Object;)Z"),
        source_file: None,
        code: Arc::from(code.as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 8,
        max_locals: 1,
        num_params: 1,
        is_synchronized: false,
        is_static: true,
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

/// Compile `instanceof <target>` through the OPTIMIZING tier, with cp 1
/// resolved (loaded) to `target_id`.
fn compile_ir(
    method: &str,
    target: &'static str,
    target_id: u32,
    helpers: &JitRuntimeHelpers,
) -> CompiledMethod {
    // Routing, not policy: see `ir_vs_singlepass.rs::routing_not_policy`.
    cratonvm_jit::ir_evidence::force_accept_always_for_this_process();
    let cm = instanceof_method(method);
    let names = move |cp: u16| (cp == 1).then(|| target.to_string());
    let loaded = move |cp: u16| {
        (cp == 1).then_some(JitNewSite::Resolved {
            class_id: target_id,
            num_fields: 0,
            has_prim_init: false,
            has_finalizer: false,
        })
    };
    let mut req = CompileRequest::new(&cm, helpers);
    req.cp_class_name_resolver = Some(&names);
    req.cp_new_resolver = Some(&loaded);
    req.optimize = true;
    let compiled = try_compile_request(&req).expect("instanceof must compile");
    assert!(
        compiled.used_ir_backend,
        "{method}: the optimizing tier must compile an instanceof-only method \
         against a loaded target, or this test measures the single-pass body"
    );
    compiled
}

/// A header-only stand-in for a heap object: the class id at offset 0 and the
/// KIND_TAGS byte, all else zero.
fn fake_header(class_id: u32, kind_tags: u8) -> Box<[u64; 8]> {
    let kind_off = cratonvm_types::KIND_TAGS_BYTE_OFFSET;
    assert!((4..64).contains(&kind_off));
    let mut words = Box::new([0u64; 8]);
    let base = words.as_mut_ptr() as *mut u8; // Cast: byte view of the header words
                                              // SAFETY: `base` addresses 64 owned bytes and both writes are in range.
    unsafe {
        std::ptr::copy_nonoverlapping(class_id.to_le_bytes().as_ptr(), base, 4);
        *base.add(kind_off) = kind_tags;
    }
    words
}

/// Run on `obj` (0 = null) and return (answer, helper calls made).
fn run(compiled: &CompiledMethod, obj: i64) -> (i64, usize) {
    let dummy_vm = [0u8; 64];
    let before = HITS.load(Ordering::SeqCst);
    // SAFETY: JIT code compiled by this test from valid bytecode; the argument
    // is null or a live fake header the guard only reads, and the only helper
    // it can reach is the marker above, which ignores the context pointer.
    let got = unsafe {
        if compiled.needs_context() {
            compiled.try_call_with_context(dummy_vm.as_ptr() as i64, &[obj])
        } else {
            compiled.try_call(&[obj])
        }
    }
    .expect("jit call");
    (got, HITS.load(Ordering::SeqCst) - before)
}

fn addr(h: &[u64; 8]) -> i64 {
    h.as_ptr() as i64 // Cast: object address as JIT argument
}

#[test]
fn the_optimizing_tier_answers_instanceof_inline_where_the_single_pass_tier_does() {
    if !cratonvm_jit::x64::checkcast_inline_enabled() {
        return; // `CRATONVM_JIT_CHECKCAST_INLINE=0`: nothing inline to assert.
    }
    let final_miss_on =
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_INSTANCEOF_FINAL_MISS")
            && !cratonvm_types::duplicate_class_name_seen();
    let helpers = helpers();

    // -- a final boot class --------------------------------------------------
    const STRING_ID: u32 = 0x0009_1101;
    let s = compile_ir("fStr", "java/lang/String", STRING_ID, &helpers);
    let exact = fake_header(STRING_ID, 0);
    let other = fake_header(STRING_ID + 1, 0);
    let proxy = fake_header(0x8000_0001, 0);
    let array = fake_header(STRING_ID, 0x01);
    assert_eq!(run(&s, 0), (0, 0), "null is answered inline");
    assert_eq!(run(&s, addr(&exact)), (1, 0), "the exact class is inline");
    if final_miss_on {
        assert_eq!(
            run(&s, addr(&other)),
            (0, 0),
            "a final boot class's miss is answered inline"
        );
    } else {
        assert_eq!(run(&s, addr(&other)), (1, 1), "opted out: the helper");
    }
    assert_eq!(
        run(&s, addr(&proxy)),
        (1, 1),
        "an id >= 0x8000_0000 asks the helper"
    );
    assert_eq!(
        run(&s, addr(&array)),
        (1, 1),
        "an array header never reaches the id compare"
    );

    // -- a non-final user class ---------------------------------------------
    const USER_ID: u32 = 0x0009_1201;
    let u = compile_ir("fUser", "com/example/r9w9/Open", USER_ID, &helpers);
    let exact = fake_header(USER_ID, 0);
    let other = fake_header(USER_ID + 1, 0);
    assert_eq!(run(&u, 0), (0, 0), "null is answered inline");
    assert_eq!(run(&u, addr(&exact)), (1, 0), "the exact class is inline");
    assert_eq!(
        run(&u, addr(&other)),
        (1, 1),
        "a site not proved final keeps asking the helper on a miss"
    );

    // -- a 1-D primitive array -----------------------------------------------
    let tag = cratonvm_types::primitive_array_kind_tags_byte("[I").expect("[I has a tag");
    let other_tag = cratonvm_types::primitive_array_kind_tags_byte("[J").expect("[J has a tag");
    assert_ne!(tag, other_tag);
    let a = compile_ir("fIntArr", "[I", 0x0009_1301, &helpers);
    let ints = fake_header(0, tag);
    let longs = fake_header(0, other_tag);
    let plain = fake_header(0x0009_1302, 0);
    assert_eq!(run(&a, 0), (0, 0), "null is answered inline");
    assert_eq!(run(&a, addr(&ints)), (1, 0), "an int[] is answered inline");
    assert_eq!(
        run(&a, addr(&plain)),
        (0, 0),
        "a plain object is never a primitive array"
    );
    assert_eq!(
        run(&a, addr(&longs)),
        (1, 1),
        "another array kind asks the helper"
    );
}
