// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 9, wave 7, lane `putstatic7` — executed tests for
//! `putstatic-sub-int-values-are-never-narrowed`.
//!
//! JVMS §6.5 `putstatic`: a `boolean` static stores `value & 1`, and a
//! `byte`/`char`/`short` static only its own width. `jit_putstatic_int` stores
//! its operand verbatim (it has no descriptor), so the single-pass `0xb3` arms
//! narrow the value register before the call. These tests compile
//! `static int set(int x) { S = x; return 7; }` with `S` of each int-family
//! descriptor, run it, and check the value the helper RECEIVED against the
//! JVMS table `jvms_static_store` — the same table the interpreter's unit test
//! (`field_access.rs`, `r9w7_putstatic7_tests`) pins `pop_static_field_value`
//! against, so the two tiers provably store the same int.
//!
//! The helper entries are recording stubs that never panic (an `extern "C"`
//! panic aborts the whole test binary).

use cratonvm_jit::JitRuntimeHelpers;
use cratonvm_jit::{try_compile, CachedBytecodeMethod, CompiledMethod, InlineSite};
use cratonvm_types::ClassId;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// One region spanning `[0x1000, usize::MAX)` (the `r9w3_x64obj3_fields.rs`
/// arrangement); nothing here dereferences a receiver.
static WIDE_OPEN: [AtomicUsize; 6] = [
    AtomicUsize::new(0x1000),
    AtomicUsize::new(usize::MAX),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

/// What every helper this file does not care about answers.
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

/// The recording `putstatic_int` stand-in's last arguments. The tests that
/// use it are serialized by [`SERIAL`].
static LAST_CLASS: AtomicI64 = AtomicI64::new(-1);
static LAST_INDEX: AtomicI64 = AtomicI64::new(-1);
static LAST_VALUE: AtomicI64 = AtomicI64::new(0);
static STORES: AtomicUsize = AtomicUsize::new(0);
static SERIAL: Mutex<()> = Mutex::new(());

/// `jit_putstatic_int(vm, class_id, field_index, val) -> i64` stand-in:
/// records its arguments, returns 0 (no `<clinit>` failure).
unsafe extern "C" fn recording_putstatic_int(
    _vm: i64,
    class_id: i64,
    field_index: i64,
    val: i64,
) -> i64 {
    LAST_CLASS.store(class_id, Ordering::SeqCst);
    LAST_INDEX.store(field_index, Ordering::SeqCst);
    LAST_VALUE.store(val, Ordering::SeqCst);
    STORES.fetch_add(1, Ordering::SeqCst);
    0
}

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
        putstatic_int: recording_putstatic_int as *const () as usize,
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
        region_bounds_addr: WIDE_OPEN.as_ptr() as usize,
        read_bounds_addr: WIDE_OPEN.as_ptr() as usize,
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

fn call(m: &CompiledMethod, args: &[i64]) -> i64 {
    let dummy_vm = [0u64; 16];
    // SAFETY: JIT-compiled from valid bytecode; every reachable helper is a
    // non-panicking stub and no argument is a reference.
    unsafe {
        if m.needs_context() {
            m.try_call_with_context(dummy_vm.as_ptr() as i64, args)
        } else {
            m.try_call(args)
        }
    }
    .expect("jit call")
}

/// JVMS §6.5 `putstatic` narrowing, written out independently of the JIT's
/// `emit_narrow_to_field_tag` (and identical to the interpreter test's table).
fn jvms_static_store(tag: u8, x: i32) -> i32 {
    match tag {
        b'Z' => x & 1,
        b'B' => i32::from(x as i8),
        b'C' => i32::from(x as u16),
        b'S' => i32::from(x as i16),
        _ => x,
    }
}

const INPUTS: [i32; 12] = [
    0,
    1,
    2,
    3,
    -1,
    300,
    -129,
    0x0001_8000,
    0x0001_0000,
    0x1234_5680,
    i32::MIN,
    i32::MAX,
];

const HOLDER_CLASS: u32 = 5;
const FIELD_INDEX: usize = 3;

/// `static int set(int x) { Holder.S = x; return 7; }` —
/// `iload_0; putstatic #1; bipush 7; ireturn` — with `S` of descriptor `tag`.
fn compile_setter(tag: u8, h: &JitRuntimeHelpers) -> CompiledMethod {
    let code = vec![0x1a, 0xb3, 0x00, 0x01, 0x10, 0x07, 0xac];
    let cm = cached("r9w7/Setter", "set", "(I)I", code, 1, 1);
    let statics = move |cp: u16| -> Option<(u32, usize, u8, bool)> {
        (cp == 1).then_some((HOLDER_CLASS, FIELD_INDEX, tag, false))
    };
    try_compile(
        &cm,
        None,           // cp_class_name_resolver
        None,           // cp_field_resolver
        Some(&statics), // cp_static_field_resolver
        None,           // cp_invoke_resolver
        None,           // callee_compiler
        None,           // cp_new_resolver
        None,           // cp_ldc_resolver
        None,           // cp_ldc2w_resolver
        None,           // profile
        h,
        None,  // inline_resolver
        None,  // string_layout_resolver
        None,  // cp_invoke_class_id_resolver
        None,  // cp_elidable_init_resolver
        false, // optimize: single-pass (the IR tier refuses putstatic)
        false,
        false,
        false,
        false,
        false,
        None,
    )
    .expect("the putstatic setter compiles single-pass")
}

/// The top-level single-pass `0xb3` arm hands `jit_putstatic_int` the
/// JVMS-narrowed value for `Z`/`B`/`C`/`S` and the untouched int for `I`.
#[test]
fn single_pass_putstatic_narrows_sub_int_values() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let h = helpers();
    for &tag in b"ZBCSI" {
        let m = compile_setter(tag, &h);
        for &x in &INPUTS {
            let before = STORES.load(Ordering::SeqCst);
            let r = call(&m, &[i64::from(x)]);
            assert_eq!(r as i32, 7, "set() must return 7 after the store");
            assert_eq!(
                STORES.load(Ordering::SeqCst),
                before + 1,
                "exactly one jit_putstatic_int call per store"
            );
            assert_eq!(LAST_CLASS.load(Ordering::SeqCst), i64::from(HOLDER_CLASS));
            assert_eq!(LAST_INDEX.load(Ordering::SeqCst), FIELD_INDEX as i64);
            assert_eq!(
                LAST_VALUE.load(Ordering::SeqCst) as i32,
                jvms_static_store(tag, x),
                "putstatic {}:{x:#x} stored the wrong int",
                tag as char
            );
        }
    }
}

/// The spliced-callee `0xb3` arm (`x64/inlining.rs`) narrows too, so a
/// `putstatic` inlined into a caller stores what the top-level arm and the
/// interpreter store. Skips (rather than failing) when the inliner declines
/// the splice — the top-level test above is the one that must always run.
#[test]
fn spliced_putstatic_narrows_sub_int_values() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let h = helpers();
    for &tag in b"ZBCS" {
        let callee = vec![0x1a, 0xb3, 0x00, 0x01, 0x10, 0x07, 0xac, 0x00, 0x00];
        let site = InlineSite {
            callee_code_len: callee.len() - 2,
            callee_code: callee,
            callee_max_locals: 1,
            callee_num_args: 1,
            callee_is_static: true,
            return_type: b'I',
            static_field_info: vec![(1, HOLDER_CLASS, FIELD_INDEX, tag, false)],
            needs_heap: true,
            class_name: "r9w7/Holder".to_string(),
            method_name: "set".to_string(),
            descriptor: "(I)I".to_string(),
            ..InlineSite::default()
        };
        // `static int run(int x) { return Holder.set(x); }`
        let cm = cached(
            "r9w7/Caller",
            "run",
            "(I)I",
            vec![0x1a, 0xb8, 0x00, 0x01, 0xac],
            1,
            1,
        );
        let invoke = |idx: u16| -> Option<(String, String, String)> {
            (idx == 1).then(|| {
                (
                    "r9w7/Holder".to_string(),
                    "set".to_string(),
                    "(I)I".to_string(),
                )
            })
        };
        let inline = |c: &str, m: &str, _d: &str| -> Option<InlineSite> {
            (c == "r9w7/Holder" && m == "set").then(|| site.clone())
        };
        let Some(m) = try_compile(
            &cm,
            None,
            None,
            None,
            Some(&invoke),
            None,
            None,
            None,
            None,
            None,
            &h,
            Some(&inline),
            None,
            None,
            None,
            false,
            false,
            false,
            false,
            false,
            false,
            None,
        ) else {
            eprintln!("spliced putstatic: caller not compiled, nothing to check");
            return;
        };
        for &x in &INPUTS {
            let before = STORES.load(Ordering::SeqCst);
            let r = call(&m, &[i64::from(x)]);
            if r == BENIGN {
                eprintln!("spliced putstatic: the callee was not spliced, nothing to check");
                return;
            }
            assert_eq!(r as i32, 7);
            assert_eq!(STORES.load(Ordering::SeqCst), before + 1);
            assert_eq!(
                LAST_VALUE.load(Ordering::SeqCst) as i32,
                jvms_static_store(tag, x),
                "spliced putstatic {}:{x:#x} stored the wrong int",
                tag as char
            );
        }
    }
}
