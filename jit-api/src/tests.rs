use cratonvm_reader::attribute::ExceptionTableEntry;
use super::*;
use std::sync::Arc;

fn make_cached_method() -> CachedBytecodeMethod {
    CachedBytecodeMethod {
        declaring_class_id: ClassId::new(1),
        class_name: Arc::from("java/lang/Object"),
        method_name: Arc::from("hashCode"),
        method_descriptor: Arc::from("()I"),
        source_file: Some(Arc::from("Object.java")),
        code: Arc::from(vec![0xB1u8].as_slice()),
        exception_table: Arc::from(vec![].as_slice()),
        max_stack: 2,
        max_locals: 1,
        num_params: 0,
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
        array_inc_declined: std::sync::OnceLock::new(),
        ctor_verdict_memo: std::sync::OnceLock::new(),
        pool_generation: u64::MAX,
    }
}

/// `from_parts` carries every decided field through and leaves every memo
/// cell exactly as the hand-written literal above does.
#[test]
fn from_parts_matches_the_hand_written_literal() {
    let literal = make_cached_method();
    let built = CachedBytecodeMethod::from_parts(CachedMethodParts {
        declaring_class_id: ClassId::new(1),
        class_name: Arc::from("java/lang/Object"),
        method_name: Arc::from("hashCode"),
        method_descriptor: Arc::from("()I"),
        source_file: Some(Arc::from("Object.java")),
        code: Arc::from(vec![0xB1u8].as_slice()),
        exception_table: Arc::from(vec![].as_slice()),
        max_stack: 2,
        max_locals: 1,
        num_params: 0,
        is_synchronized: false,
        is_static: false,
    });
    assert_eq!(built.declaring_class_id, literal.declaring_class_id);
    assert_eq!(&*built.class_name, &*literal.class_name);
    assert_eq!(&*built.method_name, &*literal.method_name);
    assert_eq!(&*built.method_descriptor, &*literal.method_descriptor);
    assert_eq!(built.source_file.as_deref(), literal.source_file.as_deref());
    assert_eq!(&*built.code, &*literal.code);
    assert!(built.exception_table.is_empty());
    assert_eq!(
        (built.max_stack, built.max_locals, built.num_params),
        (2, 1, 0)
    );
    assert!(!built.is_synchronized && !built.is_static);
    assert!(built.force_native_cache.get().is_none());
    assert!(built.descriptor_facts_cache.get().is_none());
    assert!(built.intercept_shape_cache.get().is_none());
    assert!(built.native_callback_cache.get().is_none());
    assert!(built.invoc_key.get().is_none());
    assert!(built.quickened.get().is_none());
    assert!(built.array_inc_declined.get().is_none());
    let relaxed = std::sync::atomic::Ordering::Relaxed;
    assert_eq!(built.interp_invocations.load(relaxed), 0);
    assert_eq!(built.tiering_settled.load(relaxed), 0);
    assert!(!built.branch_profile_armed.load(relaxed));
    assert_eq!(built.jit_probe_generation.load(relaxed), 0);
}


#[test]
fn jit_probe_generation_memo_reports_current_only_for_the_recorded_value() {
    let cached = make_cached_method();
    // A fresh entry has never probed: 0 can never equal a live generation
    // (a cache generation starts at 1 and only increases).
    assert!(!cached.jit_probe_is_current(1));
    assert!(!cached.jit_probe_is_current(u64::MAX));

    cached.record_jit_probe_miss(17);
    // Fast path engages only for the exact generation the probe ran at.
    assert!(cached.jit_probe_is_current(17));
    assert!(!cached.jit_probe_is_current(18));
    assert!(!cached.jit_probe_is_current(16));

    // A later probe at a newer generation replaces the memo.
    cached.record_jit_probe_miss(18);
    assert!(cached.jit_probe_is_current(18));
    assert!(!cached.jit_probe_is_current(17));
}

/// The manual `Clone` impl must carry the memos forward, not silently drop
/// them (a dropped memo is only a perf regression, but a *wrong* one would
/// be a correctness bug, so pin the exact values).
#[test]
fn clone_preserves_the_memo_fields() {
    let cached = make_cached_method();
    cached.record_jit_probe_miss(99);
    let key = cached.invoc_key();
    let _ = cached.force_native_cache.set(true);

    let copy = cached.clone();
    assert!(copy.jit_probe_is_current(99));
    assert_eq!(copy.invoc_key(), key);
    assert_eq!(copy.force_native_cache.get(), Some(&true));

    // The clone's atomic is independent of the original's.
    copy.record_jit_probe_miss(100);
    assert!(cached.jit_probe_is_current(99));
    assert!(copy.jit_probe_is_current(100));
}

fn make_helpers() -> JitRuntimeHelpers {
    JitRuntimeHelpers {
        newarray: 0x1000,
        new_object: 0x1008,
        anewarray_object: 0x1010,
        baload: 0x1018,
        bastore: 0x1020,
        iaload: 0x1028,
        iastore: 0x1030,
        aaload: 0x1038,
        aastore: 0x1040,
        multianewarray_2d: 0x1048,
        arraylength: 0x1050,
        getfield: 0x1058,
        putfield_int: 0x1060,
        putfield_long: 0x1068,
        putfield_float: 0x1070,
        putfield_double: 0x1078,
        putfield_object: 0x1080,
        getstatic: 0x1088,
        putstatic_int: 0x1090,
        putstatic_long: 0x1098,
        putstatic_float: 0x10A0,
        putstatic_double: 0x10A8,
        putstatic_object: 0x10B0,
        checkcast: 0x10B8,
        instanceof_check: 0x10C0,
        throw_aioobe: 0x10C8,
        throw_arithmetic: 0x1128,
        invoke_dispatch: 0x10D0,
        invoke_virtual_mic: 0x10D8,
        lambda_int_to_double: 0x10DC,
        write_barrier: 0x10E0,
        satb_pre_write_barrier: 0x1110,
        uncommon_trap: 0x10E8,
        math_fma_double: 0x10F0,
        math_fma_float: 0x10F8,
        tlab_cursor_offset_in_thread: 0,
        tlab_end_offset_in_thread: 8,
        class_id_offset_in_obj: 0,
        get_current_thread: 0x1100,
        tlab_post_init: 0x1108,
        frame_record: 0x1110,
        shadow_stack_offset_in_thread: 0,
        throw_exception: 0x1118,
        jit_npe_with_action: 0x1120,
        dispatch_threw: 0x1128,
        jit_frem: 0x1130,
        jit_drem: 0x1138,
        self_call_stack_guard: 0x1140,
        region_bounds_addr: 0x1148,
        native_stack_floor_fn: 0x1150,
        ldc_string: 0x1158,
        safepoint_flag_addr: 0x1160,
        safepoint_slow_path: 0x1168,
        jit_card_table_addr: 0x1170,
        jit_card_old_base: 0x1178,
        jit_card_old_end: 0x1180,
        set_throw_bci: 0x1188,
        service_callee_deopt: 0x1190,
        new_object_cp: 0x1198,
        anewarray_object_cp: 0x11A0,
        monitor_enter: 0x11A8,
        monitor_exit: 0x11B0,
        ldc_class_cp: 0x11B8,
        aastore_type_check: 0x11C0,
        read_bounds_addr: 0x11C8,
        local_handler_lookup: 0x11D0,
        ldc_string_cp: 0x11D8,
        ffm_segment_get: 0x11E0,
        ffm_segment_set: 0x11E8,
        ref_store_pre_gate: 0x11F0,
        ref_store_post_gate: 0x11F8,
        ref_store_post_young_floor: 0x1200,
        g1_barrier_addr: 0x1208,
        g1_post_write_barrier: 0x1210,
        ref_store_post_skip_mask: 0x1,
        zgc_note_tlab_object: 0x1218,
        multianewarray_n: 0x1220,
        monitor_block_offset_in_thread: 0,
        jit_stack_floor_offset_in_thread: 0,
    }
}

// --- CachedBytecodeMethod ---

#[test]
fn test_cached_method_construction() {
    let m = make_cached_method();
    assert_eq!(m.declaring_class_id, ClassId::new(1));
    assert_eq!(&*m.class_name, "java/lang/Object");
    assert_eq!(&*m.method_name, "hashCode");
    assert_eq!(&*m.method_descriptor, "()I");
    assert_eq!(m.max_stack, 2);
    assert_eq!(m.max_locals, 1);
    assert_eq!(m.num_params, 0);
}

#[test]
fn test_cached_method_source_file_some() {
    let m = make_cached_method();
    assert!(m.source_file.is_some());
    assert_eq!(&*m.source_file.unwrap(), "Object.java");
}

#[test]
fn test_cached_method_source_file_none() {
    let m = CachedBytecodeMethod {
        source_file: None,
        ..make_cached_method()
    };
    assert!(m.source_file.is_none());
}

#[test]
fn test_cached_method_code_content() {
    let m = make_cached_method();
    assert_eq!(m.code.len(), 1);
    assert_eq!(m.code[0], 0xB1); // return opcode
}

#[test]
fn test_cached_method_empty_exception_table() {
    let m = make_cached_method();
    assert!(m.exception_table.is_empty());
}

#[test]
fn test_cached_method_with_exception_table() {
    let entry = ExceptionTableEntry {
        start_pc: 0,
        end_pc: 10,
        handler_pc: 20,
        catch_type: 5,
    };
    let m = CachedBytecodeMethod {
        exception_table: Arc::from(vec![entry].as_slice()),
        ..make_cached_method()
    };
    assert_eq!(m.exception_table.len(), 1);
    assert_eq!(m.exception_table[0].start_pc, 0);
    assert_eq!(m.exception_table[0].end_pc, 10);
    assert_eq!(m.exception_table[0].handler_pc, 20);
    assert_eq!(m.exception_table[0].catch_type, 5);
}

#[test]
fn test_cached_method_clone() {
    let m1 = make_cached_method();
    let m2 = m1.clone();
    assert_eq!(&*m1.class_name, &*m2.class_name);
    assert_eq!(&*m1.method_name, &*m2.method_name);
    assert_eq!(m1.max_stack, m2.max_stack);
    assert_eq!(m1.max_locals, m2.max_locals);
    assert_eq!(m1.code.len(), m2.code.len());
}

#[test]
fn test_cached_method_arc_sharing() {
    let m1 = make_cached_method();
    let m2 = m1.clone();
    // Arc::clone shares the same allocation
    assert!(Arc::ptr_eq(&m1.class_name, &m2.class_name));
    assert!(Arc::ptr_eq(&m1.code, &m2.code));
}

#[test]
fn test_cached_method_large_code() {
    let code: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();
    let m = CachedBytecodeMethod {
        code: Arc::from(code.as_slice()),
        max_stack: 100,
        max_locals: 50,
        ..make_cached_method()
    };
    assert_eq!(m.code.len(), 1024);
    assert_eq!(m.max_stack, 100);
    assert_eq!(m.max_locals, 50);
}

// --- JitRuntimeHelpers ---

#[test]
fn test_helpers_construction() {
    let h = make_helpers();
    assert_eq!(h.newarray, 0x1000);
    assert_eq!(h.new_object, 0x1008);
    assert_eq!(h.write_barrier, 0x10E0);
}

#[test]
fn test_helpers_copy() {
    let h1 = make_helpers();
    let h2 = h1; // Copy
    assert_eq!(h1.newarray, h2.newarray);
    assert_eq!(h1.invoke_dispatch, h2.invoke_dispatch);
}

#[test]
fn test_helpers_clone() {
    let h1 = make_helpers();
    let h2 = h1.clone();
    assert_eq!(h1.checkcast, h2.checkcast);
    assert_eq!(h1.instanceof_check, h2.instanceof_check);
}

#[test]
fn test_helpers_all_fields_distinct() {
    let h = make_helpers();
    let ptrs = [
        h.newarray,
        h.new_object,
        h.anewarray_object,
        h.baload,
        h.bastore,
        h.iaload,
        h.iastore,
        h.aaload,
        h.aastore,
        h.multianewarray_2d,
        h.arraylength,
        h.getfield,
        h.putfield_int,
        h.putfield_long,
        h.putfield_float,
        h.putfield_double,
        h.putfield_object,
        h.getstatic,
        h.putstatic_int,
        h.putstatic_long,
        h.putstatic_float,
        h.putstatic_double,
        h.putstatic_object,
        h.checkcast,
        h.instanceof_check,
        h.throw_aioobe,
        h.invoke_dispatch,
        h.invoke_virtual_mic,
        h.lambda_int_to_double,
        h.write_barrier,
        h.satb_pre_write_barrier,
    ];
    // All addresses should be unique
    let mut set = std::collections::HashSet::new();
    for p in &ptrs {
        assert!(set.insert(p), "Duplicate pointer value: {:#x}", p);
    }
    assert_eq!(set.len(), 31);
}

#[test]
fn test_helpers_zero_values() {
    let h = JitRuntimeHelpers {
        newarray: 0,
        new_object: 0,
        anewarray_object: 0,
        baload: 0,
        bastore: 0,
        iaload: 0,
        iastore: 0,
        aaload: 0,
        aastore: 0,
        multianewarray_2d: 0,
        arraylength: 0,
        getfield: 0,
        putfield_int: 0,
        putfield_long: 0,
        putfield_float: 0,
        putfield_double: 0,
        putfield_object: 0,
        getstatic: 0,
        putstatic_int: 0,
        putstatic_long: 0,
        putstatic_float: 0,
        putstatic_double: 0,
        putstatic_object: 0,
        checkcast: 0,
        instanceof_check: 0,
        throw_aioobe: 0,
        throw_arithmetic: 0,
        invoke_dispatch: 0,
        invoke_virtual_mic: 0,
        lambda_int_to_double: 0,
        write_barrier: 0,
        satb_pre_write_barrier: 0,
        uncommon_trap: 0,
        math_fma_double: 0,
        math_fma_float: 0,
        tlab_cursor_offset_in_thread: 0,
        tlab_end_offset_in_thread: 0,
        class_id_offset_in_obj: 0,
        get_current_thread: 0,
        tlab_post_init: 0,
        frame_record: 0,
        shadow_stack_offset_in_thread: 0,
        throw_exception: 0,
        jit_npe_with_action: 0,
        dispatch_threw: 0,
        jit_frem: 0,
        jit_drem: 0,
        self_call_stack_guard: 0,
        region_bounds_addr: 0,
        native_stack_floor_fn: 0,
        ldc_string: 0,
        safepoint_flag_addr: 0,
        safepoint_slow_path: 0,
        jit_card_table_addr: 0,
        jit_card_old_base: 0,
        jit_card_old_end: 0,
        set_throw_bci: 0,
        service_callee_deopt: 0,
        new_object_cp: 0,
        anewarray_object_cp: 0,
        monitor_enter: 0,
        monitor_exit: 0,
        ldc_class_cp: 0,
        aastore_type_check: 0,
        read_bounds_addr: 0,
        local_handler_lookup: 0,
        ldc_string_cp: 0,
        ffm_segment_get: 0,
        ffm_segment_set: 0,
        ref_store_pre_gate: 0,
        ref_store_post_gate: 0,
        ref_store_post_young_floor: 0,
        g1_barrier_addr: 0,
        g1_post_write_barrier: 0,
        ref_store_post_skip_mask: 0,
        zgc_note_tlab_object: 0,
        multianewarray_n: 0,
        monitor_block_offset_in_thread: 0,
        jit_stack_floor_offset_in_thread: 0,
    };
    assert_eq!(h.newarray, 0);
    assert_eq!(h.write_barrier, 0);
}

#[test]
fn test_helpers_field_access_all() {
    let h = make_helpers();
    // Verify every field is accessible and has the expected value
    assert_eq!(h.anewarray_object, 0x1010);
    assert_eq!(h.baload, 0x1018);
    assert_eq!(h.bastore, 0x1020);
    assert_eq!(h.iaload, 0x1028);
    assert_eq!(h.iastore, 0x1030);
    assert_eq!(h.aaload, 0x1038);
    assert_eq!(h.aastore, 0x1040);
    assert_eq!(h.multianewarray_2d, 0x1048);
    assert_eq!(h.arraylength, 0x1050);
    assert_eq!(h.getfield, 0x1058);
    assert_eq!(h.putfield_int, 0x1060);
    assert_eq!(h.putfield_long, 0x1068);
    assert_eq!(h.putfield_float, 0x1070);
    assert_eq!(h.putfield_double, 0x1078);
    assert_eq!(h.putfield_object, 0x1080);
    assert_eq!(h.getstatic, 0x1088);
    assert_eq!(h.putstatic_int, 0x1090);
    assert_eq!(h.putstatic_long, 0x1098);
    assert_eq!(h.putstatic_float, 0x10A0);
    assert_eq!(h.putstatic_double, 0x10A8);
    assert_eq!(h.putstatic_object, 0x10B0);
    assert_eq!(h.checkcast, 0x10B8);
    assert_eq!(h.instanceof_check, 0x10C0);
    assert_eq!(h.throw_aioobe, 0x10C8);
    assert_eq!(h.invoke_dispatch, 0x10D0);
    assert_eq!(h.invoke_virtual_mic, 0x10D8);
}

#[test]
fn test_cached_method_multiple_exception_entries() {
    let entries = vec![
        ExceptionTableEntry {
            start_pc: 0,
            end_pc: 5,
            handler_pc: 10,
            catch_type: 1,
        },
        ExceptionTableEntry {
            start_pc: 5,
            end_pc: 15,
            handler_pc: 20,
            catch_type: 2,
        },
        ExceptionTableEntry {
            start_pc: 0,
            end_pc: 15,
            handler_pc: 30,
            catch_type: 0,
        }, // finally
    ];
    let m = CachedBytecodeMethod {
        exception_table: Arc::from(entries.as_slice()),
        ..make_cached_method()
    };
    assert_eq!(m.exception_table.len(), 3);
    assert_eq!(m.exception_table[2].catch_type, 0); // finally handler
}

#[test]
fn test_cached_method_declaring_class_id_equality() {
    let m1 = make_cached_method();
    let m2 = CachedBytecodeMethod {
        declaring_class_id: ClassId::new(2),
        ..make_cached_method()
    };
    assert_eq!(m1.declaring_class_id, ClassId::new(1));
    assert_eq!(m2.declaring_class_id, ClassId::new(2));
    assert_ne!(m1.declaring_class_id, m2.declaring_class_id);
}

// --- JitRuntimeHelpers validation ---

#[test]
fn test_helpers_validate_all_nonzero() {
    let h = make_helpers();
    assert!(h.validate().is_ok());
}

#[test]
fn test_helpers_validate_fails_on_zero() {
    let mut h = make_helpers();
    h.newarray = 0;
    let err = h.validate().expect_err("zero newarray must fail");
    assert!(err.contains(&"newarray"));
}

#[test]
fn test_helpers_null_pointers_empty_when_valid() {
    let h = make_helpers();
    assert!(h.null_pointers().is_empty());
}

#[test]
fn test_helpers_null_pointers_reports_zeroed_fields() {
    let mut h = make_helpers();
    h.newarray = 0;
    h.write_barrier = 0;
    let nulls = h.null_pointers();
    assert!(nulls.contains(&"newarray"));
    assert!(nulls.contains(&"write_barrier"));
    assert_eq!(nulls.len(), 2);
}

// --- Builder tests deleted in 2026-05-16 audit: Builder itself
//     was deleted (see comment above the deleted block in the
//     module body). The remaining tests cover the runtime-helpers
//     struct + validation directly.

// --- JitRuntimeHelpers golden ABI offsets ---
//
// The JIT compiler bakes `JitRuntimeHelpers` slot values into
// generated RWX machine code as absolute CALL targets, and
// `HELPER_FIELDS` plus the `Offset` slots name slots by byte offset.
// A silent field reorder would leave those offsets naming the old
// slots — i.e. a CALL that used to dispatch `new_object` would
// now dispatch `anewarray_object`, with no compile error. This
// failure is undetectable at runtime until the wrong helper
// corrupts the heap.
//
// The struct is `#[repr(C)]` and every field is `usize` (8 bytes
// on x86-64, the only supported target). That gives a sequential
// layout with no padding, so the documented offset of field N is
// simply `N * 8`. The test below pins both:
//
//   1. The byte offset of every individual field (via
//      `std::mem::offset_of!`, stable since 1.77), and
//   2. The 0-based field index of every field (via a const N) so
//      a swap of two fields with the same size — which preserves
//      every offset individually — is also caught.
//
// If you add a new field to `JitRuntimeHelpers`, append a row
// below and bump the `NUM_FIELDS` literal in the const-assert in
// the module body. Do NOT renumber existing rows: the indices
// here are part of the JIT ABI.

/// Width of every field in `JitRuntimeHelpers` (every field is `usize`).
/// On any 64-bit target Rust supports this is 8. We pin to 8 because the
/// JIT only emits x86-64 code today; if/when 32-bit support lands, this
/// test (and the JIT immediate-width assumptions) must be revisited.
const FIELD_WIDTH: usize = 8;

/// Documented JIT ABI offset of a field, given its 0-based index in
/// the `#[repr(C)]` declaration order. Sequential because every
/// field is `usize` (no inter-field padding).
const fn expected_offset(index: usize) -> usize {
    index * FIELD_WIDTH
}

#[test]
fn jit_runtime_helpers_field_size_is_eight() {
    // x86-64 only. If this fires, the per-field 8-byte stride
    // below is no longer the JIT ABI and the offset assertions
    // are stale.
    assert_eq!(std::mem::size_of::<usize>(), FIELD_WIDTH);
}

#[test]
fn jit_runtime_helpers_struct_size_matches_field_count() {
    // `#[repr(C)]` + all `usize` fields ⇒ no padding ⇒ struct size
    // is exactly `NUM_FIELDS * 8`. Drift here means a field was
    // added/removed without updating `helper_fields!`, OR the
    // struct gained a non-`usize` field that violates the JIT ABI.
    assert_eq!(
        std::mem::size_of::<JitRuntimeHelpers>(),
        JitRuntimeHelpers::NUM_FIELDS * FIELD_WIDTH,
    );
    // And the macro-driven count is the canonical one for this ABI
    // revision -- 76 as of v13. v10 appended dev's three reference-store
    // barrier gates; F-08 appended the two G1 inline-barrier words after
    // them; v12 appended the generational collector's post-barrier skip
    // mask; v13 appended the ZGC inline-TLAB announce helper, so every
    // earlier golden offset below stays where it was.
    // v14 appended the N-dimensional `multianewarray` helper; v15 the
    // inline thin-lock's `JitMonitorBlock` displacement; v16 the
    // native-stack floor word's displacement.
    assert_eq!(JitRuntimeHelpers::NUM_FIELDS, 79);
}

#[test]
fn jit_runtime_helpers_repr_c_golden_offsets() {
    // Each row: (0-based ABI index, field name, offset_of! probe).
    // The expected byte offset is `index * 8`. The names are also
    // checked against `all_fields()` so a field rename (which the
    // offset_of! invocation would silently follow) is caught too.
    let probes: [(usize, &'static str, usize); JitRuntimeHelpers::NUM_FIELDS] = [
        (
            0,
            "newarray",
            std::mem::offset_of!(JitRuntimeHelpers, newarray),
        ),
        (
            1,
            "new_object",
            std::mem::offset_of!(JitRuntimeHelpers, new_object),
        ),
        (
            2,
            "anewarray_object",
            std::mem::offset_of!(JitRuntimeHelpers, anewarray_object),
        ),
        (3, "baload", std::mem::offset_of!(JitRuntimeHelpers, baload)),
        (
            4,
            "bastore",
            std::mem::offset_of!(JitRuntimeHelpers, bastore),
        ),
        (5, "iaload", std::mem::offset_of!(JitRuntimeHelpers, iaload)),
        (
            6,
            "iastore",
            std::mem::offset_of!(JitRuntimeHelpers, iastore),
        ),
        (7, "aaload", std::mem::offset_of!(JitRuntimeHelpers, aaload)),
        (
            8,
            "aastore",
            std::mem::offset_of!(JitRuntimeHelpers, aastore),
        ),
        (
            9,
            "multianewarray_2d",
            std::mem::offset_of!(JitRuntimeHelpers, multianewarray_2d),
        ),
        (
            10,
            "arraylength",
            std::mem::offset_of!(JitRuntimeHelpers, arraylength),
        ),
        (
            11,
            "getfield",
            std::mem::offset_of!(JitRuntimeHelpers, getfield),
        ),
        (
            12,
            "putfield_int",
            std::mem::offset_of!(JitRuntimeHelpers, putfield_int),
        ),
        (
            13,
            "putfield_long",
            std::mem::offset_of!(JitRuntimeHelpers, putfield_long),
        ),
        (
            14,
            "putfield_float",
            std::mem::offset_of!(JitRuntimeHelpers, putfield_float),
        ),
        (
            15,
            "putfield_double",
            std::mem::offset_of!(JitRuntimeHelpers, putfield_double),
        ),
        (
            16,
            "putfield_object",
            std::mem::offset_of!(JitRuntimeHelpers, putfield_object),
        ),
        (
            17,
            "getstatic",
            std::mem::offset_of!(JitRuntimeHelpers, getstatic),
        ),
        (
            18,
            "putstatic_int",
            std::mem::offset_of!(JitRuntimeHelpers, putstatic_int),
        ),
        (
            19,
            "putstatic_long",
            std::mem::offset_of!(JitRuntimeHelpers, putstatic_long),
        ),
        (
            20,
            "putstatic_float",
            std::mem::offset_of!(JitRuntimeHelpers, putstatic_float),
        ),
        (
            21,
            "putstatic_double",
            std::mem::offset_of!(JitRuntimeHelpers, putstatic_double),
        ),
        (
            22,
            "putstatic_object",
            std::mem::offset_of!(JitRuntimeHelpers, putstatic_object),
        ),
        (
            23,
            "checkcast",
            std::mem::offset_of!(JitRuntimeHelpers, checkcast),
        ),
        (
            24,
            "instanceof_check",
            std::mem::offset_of!(JitRuntimeHelpers, instanceof_check),
        ),
        (
            25,
            "throw_aioobe",
            std::mem::offset_of!(JitRuntimeHelpers, throw_aioobe),
        ),
        (
            26,
            "throw_arithmetic",
            std::mem::offset_of!(JitRuntimeHelpers, throw_arithmetic),
        ),
        (
            27,
            "invoke_dispatch",
            std::mem::offset_of!(JitRuntimeHelpers, invoke_dispatch),
        ),
        (
            28,
            "invoke_virtual_mic",
            std::mem::offset_of!(JitRuntimeHelpers, invoke_virtual_mic),
        ),
        (
            29,
            "lambda_int_to_double",
            std::mem::offset_of!(JitRuntimeHelpers, lambda_int_to_double),
        ),
        (
            30,
            "write_barrier",
            std::mem::offset_of!(JitRuntimeHelpers, write_barrier),
        ),
        (
            31,
            "satb_pre_write_barrier",
            std::mem::offset_of!(JitRuntimeHelpers, satb_pre_write_barrier),
        ),
        (
            32,
            "uncommon_trap",
            std::mem::offset_of!(JitRuntimeHelpers, uncommon_trap),
        ),
        (
            33,
            "math_fma_double",
            std::mem::offset_of!(JitRuntimeHelpers, math_fma_double),
        ),
        (
            34,
            "math_fma_float",
            std::mem::offset_of!(JitRuntimeHelpers, math_fma_float),
        ),
        (
            35,
            "tlab_cursor_offset_in_thread",
            std::mem::offset_of!(JitRuntimeHelpers, tlab_cursor_offset_in_thread),
        ),
        (
            36,
            "tlab_end_offset_in_thread",
            std::mem::offset_of!(JitRuntimeHelpers, tlab_end_offset_in_thread),
        ),
        (
            37,
            "class_id_offset_in_obj",
            std::mem::offset_of!(JitRuntimeHelpers, class_id_offset_in_obj),
        ),
        (
            38,
            "get_current_thread",
            std::mem::offset_of!(JitRuntimeHelpers, get_current_thread),
        ),
        (
            39,
            "tlab_post_init",
            std::mem::offset_of!(JitRuntimeHelpers, tlab_post_init),
        ),
        (
            40,
            "frame_record",
            std::mem::offset_of!(JitRuntimeHelpers, frame_record),
        ),
        (
            41,
            "shadow_stack_offset_in_thread",
            std::mem::offset_of!(JitRuntimeHelpers, shadow_stack_offset_in_thread),
        ),
        (
            42,
            "throw_exception",
            std::mem::offset_of!(JitRuntimeHelpers, throw_exception),
        ),
        (
            43,
            "jit_npe_with_action",
            std::mem::offset_of!(JitRuntimeHelpers, jit_npe_with_action),
        ),
        (
            44,
            "dispatch_threw",
            std::mem::offset_of!(JitRuntimeHelpers, dispatch_threw),
        ),
        (
            45,
            "jit_frem",
            std::mem::offset_of!(JitRuntimeHelpers, jit_frem),
        ),
        (
            46,
            "jit_drem",
            std::mem::offset_of!(JitRuntimeHelpers, jit_drem),
        ),
        (
            47,
            "self_call_stack_guard",
            std::mem::offset_of!(JitRuntimeHelpers, self_call_stack_guard),
        ),
        (
            48,
            "region_bounds_addr",
            std::mem::offset_of!(JitRuntimeHelpers, region_bounds_addr),
        ),
        (
            49,
            "native_stack_floor_fn",
            std::mem::offset_of!(JitRuntimeHelpers, native_stack_floor_fn),
        ),
        (
            50,
            "ldc_string",
            std::mem::offset_of!(JitRuntimeHelpers, ldc_string),
        ),
        (
            51,
            "safepoint_flag_addr",
            std::mem::offset_of!(JitRuntimeHelpers, safepoint_flag_addr),
        ),
        (
            52,
            "safepoint_slow_path",
            std::mem::offset_of!(JitRuntimeHelpers, safepoint_slow_path),
        ),
        (
            53,
            "jit_card_table_addr",
            std::mem::offset_of!(JitRuntimeHelpers, jit_card_table_addr),
        ),
        (
            54,
            "jit_card_old_base",
            std::mem::offset_of!(JitRuntimeHelpers, jit_card_old_base),
        ),
        (
            55,
            "jit_card_old_end",
            std::mem::offset_of!(JitRuntimeHelpers, jit_card_old_end),
        ),
        (
            56,
            "set_throw_bci",
            std::mem::offset_of!(JitRuntimeHelpers, set_throw_bci),
        ),
        (
            57,
            "service_callee_deopt",
            std::mem::offset_of!(JitRuntimeHelpers, service_callee_deopt),
        ),
        (
            58,
            "new_object_cp",
            std::mem::offset_of!(JitRuntimeHelpers, new_object_cp),
        ),
        (
            59,
            "anewarray_object_cp",
            std::mem::offset_of!(JitRuntimeHelpers, anewarray_object_cp),
        ),
        (
            60,
            "monitor_enter",
            std::mem::offset_of!(JitRuntimeHelpers, monitor_enter),
        ),
        (
            61,
            "monitor_exit",
            std::mem::offset_of!(JitRuntimeHelpers, monitor_exit),
        ),
        (
            62,
            "ldc_class_cp",
            std::mem::offset_of!(JitRuntimeHelpers, ldc_class_cp),
        ),
        (
            63,
            "aastore_type_check",
            std::mem::offset_of!(JitRuntimeHelpers, aastore_type_check),
        ),
        (
            64,
            "read_bounds_addr",
            std::mem::offset_of!(JitRuntimeHelpers, read_bounds_addr),
        ),
        (
            65,
            "local_handler_lookup",
            std::mem::offset_of!(JitRuntimeHelpers, local_handler_lookup),
        ),
        (
            66,
            "ldc_string_cp",
            std::mem::offset_of!(JitRuntimeHelpers, ldc_string_cp),
        ),
        (
            67,
            "ffm_segment_get",
            std::mem::offset_of!(JitRuntimeHelpers, ffm_segment_get),
        ),
        (
            68,
            "ffm_segment_set",
            std::mem::offset_of!(JitRuntimeHelpers, ffm_segment_set),
        ),
        (
            69,
            "ref_store_pre_gate",
            std::mem::offset_of!(JitRuntimeHelpers, ref_store_pre_gate),
        ),
        (
            70,
            "ref_store_post_gate",
            std::mem::offset_of!(JitRuntimeHelpers, ref_store_post_gate),
        ),
        (
            71,
            "ref_store_post_young_floor",
            std::mem::offset_of!(JitRuntimeHelpers, ref_store_post_young_floor),
        ),
        (
            72,
            "g1_barrier_addr",
            std::mem::offset_of!(JitRuntimeHelpers, g1_barrier_addr),
        ),
        (
            73,
            "g1_post_write_barrier",
            std::mem::offset_of!(JitRuntimeHelpers, g1_post_write_barrier),
        ),
        (
            74,
            "ref_store_post_skip_mask",
            std::mem::offset_of!(JitRuntimeHelpers, ref_store_post_skip_mask),
        ),
        (
            75,
            "zgc_note_tlab_object",
            std::mem::offset_of!(JitRuntimeHelpers, zgc_note_tlab_object),
        ),
        (
            76,
            "multianewarray_n",
            std::mem::offset_of!(JitRuntimeHelpers, multianewarray_n),
        ),
        (
            77,
            "monitor_block_offset_in_thread",
            std::mem::offset_of!(JitRuntimeHelpers, monitor_block_offset_in_thread),
        ),
        (
            78,
            "jit_stack_floor_offset_in_thread",
            std::mem::offset_of!(JitRuntimeHelpers, jit_stack_floor_offset_in_thread),
        ),
    ];

    // (a) Each field is at its documented sequential byte offset.
    // A swap of two same-size fields would change the (index, name)
    // → offset_of! relationship and trip this.
    for (idx, name, actual) in probes.iter() {
        let expected = expected_offset(*idx);
        assert_eq!(
            *actual, expected,
            "ABI offset drift: field `{}` (index {}) is at byte offset {}, \
             expected {}. The JIT bakes this offset into RWX code — fix the \
             reorder, or if intentional, update both this table and \
             `vm/src/jit/helpers.rs::build_helpers`.",
            name, idx, actual, expected,
        );
    }

    // (b) The macro-generated name list agrees with the golden
    // table, in the same order. This catches a field rename in
    // the struct that the contributor forgot to mirror into the
    // `helper_fields!` macro invocation (or vice versa).
    let h = make_helpers();
    let fields = h.all_fields();
    assert_eq!(fields.len(), probes.len());
    for (i, (_, expected_name, _)) in probes.iter().enumerate() {
        assert_eq!(
            fields[i].name, *expected_name,
            "macro-driven field order disagrees with golden ABI table at \
             index {}: macro has `{}`, golden has `{}`",
            i, fields[i].name, expected_name,
        );
    }
}

#[test]
fn jit_runtime_helpers_all_fields_classified() {
    // The macro must classify every field.
    // 43 RequiredPtr + 12 OptionalPtr + 10 Offset = 65. A new
    // field whose classification is omitted will fail to compile (the
    // macro requires both arms); this test pins the *counts* so a
    // reclassification (e.g. demoting a RequiredPtr to OptionalPtr) is
    // also a deliberate, reviewed change.
    let h = make_helpers();
    let f = h.all_fields();
    let req = f
        .iter()
        .filter(|e| e.kind == FieldKind::RequiredPtr)
        .count();
    let opt = f
        .iter()
        .filter(|e| e.kind == FieldKind::OptionalPtr)
        .count();
    let off = f.iter().filter(|e| e.kind == FieldKind::Offset).count();
    // v14's `multianewarray_n` is a RequiredPtr: 43 -> 44.
    assert_eq!(req, 44, "required-pointer count drifted");
    // v13's `zgc_note_tlab_object` is an OptionalPtr (0 = not wired).
    assert_eq!(opt, 18, "optional-pointer count drifted");
    // v12's `ref_store_post_skip_mask` is an Offset (a baked VALUE), which
    // is why this moves and the two pointer counts do not.
    // v15's `monitor_block_offset_in_thread` is an Offset: 15 -> 16.
    // v16's `jit_stack_floor_offset_in_thread` is an Offset: 16 -> 17.
    assert_eq!(off, 17, "offset-field count drifted");
    assert_eq!(req + opt + off, JitRuntimeHelpers::NUM_FIELDS);
}

#[test]
fn jit_runtime_helpers_validate_ignores_offsets() {
    // `class_id_offset_in_obj` is 0 by contract; the validator
    // must NOT reject on that. (Regression for the round-9 fix —
    // the previous `validate()` looped over a hand-maintained array
    // that omitted all offset/optional fields, so this was true
    // by accident. The new validator iterates ALL 46 fields and
    // must still pass when offsets are zero.)
    let mut h = make_helpers();
    h.tlab_cursor_offset_in_thread = 0;
    h.tlab_end_offset_in_thread = 0;
    h.class_id_offset_in_obj = 0;
    assert!(h.validate().is_ok());
    assert!(h.null_pointers().is_empty());
}

#[test]
fn jit_runtime_helpers_validate_accepts_unwired_optionals() {
    // Optional helpers default to zero ("not wired") — must pass.
    let mut h = make_helpers();
    h.get_current_thread = 0;
    h.tlab_post_init = 0;
    assert!(h.validate().is_ok());
    // And they are NOT reported by null_pointers (they are not
    // mandatory-and-null, just optional-and-unset).
    assert!(h.null_pointers().is_empty());
}

#[test]
fn jit_runtime_helpers_validate_rejects_each_required_null() {
    // Sweep every required-pointer field: zeroing it must trip
    // the validator AND report the field by name.
    let names: Vec<&'static str> = make_helpers()
        .all_fields()
        .iter()
        .filter(|e| e.kind == FieldKind::RequiredPtr)
        .map(|e| e.name)
        .collect();
    assert_eq!(names.len(), 44);
    for name in names {
        let mut h = make_helpers();
        // Zero the field by name via a match — the macro doesn't
        // give us per-field mutable accessors, so this is exhaustive
        // but verbose. The test exists precisely to ensure no
        // required field is silently uncovered.
        zero_field_by_name(&mut h, name);
        match h.validate() {
            Ok(()) => panic!(
                "validator failed to reject null required field `{}` (returned Ok)",
                name
            ),
            Err(err) => assert!(
                err.contains(&name),
                "validator's Err list did not include `{}`; got {:?}",
                name,
                err,
            ),
        }
        let nulls = h.null_pointers();
        assert!(
            nulls.contains(&name),
            "null_pointers() did not report `{}`; got {:?}",
            name,
            nulls,
        );
    }
}

#[test]
fn jit_runtime_helpers_all_required_null_reports_every_name() {
    // Zero EVERY required pointer at once: `null_pointers()` must
    // return the complete set of 41 required-field names (and
    // `validate()` must reject). This complements the per-field
    // sweep above — it proves the validator does not stop at the
    // first miss and that the offset/optional fields (left non-zero
    // for offsets, zero for unwired optionals) never leak into the
    // report.
    let mut h = make_helpers();
    let required: Vec<&'static str> = h
        .all_fields()
        .iter()
        .filter(|e| e.kind == FieldKind::RequiredPtr)
        .map(|e| e.name)
        .collect();
    assert_eq!(required.len(), 44, "expected 44 required pointers");
    // throw_exception is the round-10 addition — pin it explicitly so
    // a regression that drops it from the required set is caught here
    // and not just by the count.
    assert!(
        required.contains(&"throw_exception"),
        "throw_exception must be a required (null-rejected) pointer",
    );
    // jit_npe_with_action is the JEP-358 inline-NPE-path addition — pin it
    // explicitly for the same reason.
    assert!(
        required.contains(&"jit_npe_with_action"),
        "jit_npe_with_action must be a required (null-rejected) pointer",
    );
    // dispatch_threw is the i64::MIN-sentinel J/D-return disambiguation
    // helper — pin it explicitly for the same reason.
    assert!(
        required.contains(&"dispatch_threw"),
        "dispatch_threw must be a required (null-rejected) pointer",
    );
    // jit_frem / jit_drem are the IR FP-tier (Slice A) fmod helpers — pin
    // them explicitly for the same reason.
    assert!(
        required.contains(&"jit_frem"),
        "jit_frem must be a required (null-rejected) pointer",
    );
    assert!(
        required.contains(&"jit_drem"),
        "jit_drem must be a required (null-rejected) pointer",
    );
    // ldc_string is the interned-String-materialization helper for
    // compiled `ldc` sites — pin it explicitly for the same reason.
    assert!(
        required.contains(&"ldc_string"),
        "ldc_string must be a required (null-rejected) pointer",
    );

    for name in &required {
        zero_field_by_name(&mut h, name);
    }

    let nulls = h.null_pointers();
    assert_eq!(
        nulls.len(),
        required.len(),
        "null_pointers() should report all {} required fields; got {:?}",
        required.len(),
        nulls,
    );
    for name in &required {
        assert!(
            nulls.contains(name),
            "null_pointers() omitted required field `{}`; got {:?}",
            name,
            nulls,
        );
    }
    // And validate() must fail with the same complete list.
    match h.validate() {
        Ok(()) => panic!("validate() returned Ok with all required pointers null"),
        Err(err) => assert_eq!(err.len(), required.len()),
    }
}

// Helper for `validate_rejects_each_required_null`: zero a single
// field by string name. Kept in the test module so production
// code doesn't grow a stringly-typed mutator.
fn zero_field_by_name(h: &mut JitRuntimeHelpers, name: &str) {
    match name {
        "newarray" => h.newarray = 0,
        "new_object" => h.new_object = 0,
        "anewarray_object" => h.anewarray_object = 0,
        "baload" => h.baload = 0,
        "bastore" => h.bastore = 0,
        "iaload" => h.iaload = 0,
        "iastore" => h.iastore = 0,
        "aaload" => h.aaload = 0,
        "aastore" => h.aastore = 0,
        "multianewarray_2d" => h.multianewarray_2d = 0,
        "arraylength" => h.arraylength = 0,
        "getfield" => h.getfield = 0,
        "putfield_int" => h.putfield_int = 0,
        "putfield_long" => h.putfield_long = 0,
        "putfield_float" => h.putfield_float = 0,
        "putfield_double" => h.putfield_double = 0,
        "putfield_object" => h.putfield_object = 0,
        "getstatic" => h.getstatic = 0,
        "putstatic_int" => h.putstatic_int = 0,
        "putstatic_long" => h.putstatic_long = 0,
        "putstatic_float" => h.putstatic_float = 0,
        "putstatic_double" => h.putstatic_double = 0,
        "putstatic_object" => h.putstatic_object = 0,
        "checkcast" => h.checkcast = 0,
        "instanceof_check" => h.instanceof_check = 0,
        "throw_aioobe" => h.throw_aioobe = 0,
        "throw_arithmetic" => h.throw_arithmetic = 0,
        "invoke_dispatch" => h.invoke_dispatch = 0,
        "invoke_virtual_mic" => h.invoke_virtual_mic = 0,
        "lambda_int_to_double" => h.lambda_int_to_double = 0,
        "write_barrier" => h.write_barrier = 0,
        "satb_pre_write_barrier" => h.satb_pre_write_barrier = 0,
        "uncommon_trap" => h.uncommon_trap = 0,
        "math_fma_double" => h.math_fma_double = 0,
        "math_fma_float" => h.math_fma_float = 0,
        // round-10: throw_exception was added as a RequiredPtr after
        // the original 33-field sweep was written. Without this arm the
        // sweep panics on it and the required-null coverage is silently
        // incomplete (the exact failure mode this test guards against).
        "throw_exception" => h.throw_exception = 0,
        "jit_npe_with_action" => h.jit_npe_with_action = 0,
        "dispatch_threw" => h.dispatch_threw = 0,
        "jit_frem" => h.jit_frem = 0,
        "jit_drem" => h.jit_drem = 0,
        "ldc_string" => h.ldc_string = 0,
        "set_throw_bci" => h.set_throw_bci = 0,
        "aastore_type_check" => h.aastore_type_check = 0,
        "read_bounds_addr" => h.read_bounds_addr = 0,
        "local_handler_lookup" => h.local_handler_lookup = 0,
        "multianewarray_n" => h.multianewarray_n = 0,
        other => panic!("unknown required-pointer field name in test: {}", other),
    }
}
