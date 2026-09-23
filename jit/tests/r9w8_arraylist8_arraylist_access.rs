// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 9, wave 8, lane `arraylist8` — the ARRAYLIST_ACCESS
//! prefix (`perf-collection-natives-cost-a-generic-native-dispatch-per-call`).
//!
//! A `List.get(int)` / `size()` site whose receiver is EXACTLY
//! `java/util/ArrayList` is answered inline, as a guarded prefix of the site's
//! unchanged dispatch; every other outcome must reach that dispatch (here
//! `invoke_dispatch`, stubbed to count and answer [`MISS`]). What these tests
//! execute, each against hand-built heap shapes:
//!
//! * a hit, on a LEGACY instance (16-byte `Value` cells) and on a COMPACT one
//!   (a registered `CompactLayout`), for `get` and for `size`;
//! * every decline the native's fast path (`al_fast_state` + an in-range index)
//!   declines: another class id, a null receiver, an array receiver, a null or
//!   primitive `elementData`, an index of `-1`, `size`, `size <= i < length` and
//!   `length`, the `values()`-view marker in the trailing capacity slot, a legacy
//!   cell whose tag is not what its payload is read as, and too few slots;
//! * a compile with no ArrayList layout never answers inline (the control arm),
//!   and the kill switch refuses the layout.

use cratonvm_jit::x64::compile_with_param_slots;
use cratonvm_jit::{
    try_resolve_arraylist_intrinsic, ArrayListFieldLayout, CompiledMethod, JitIntrinsic,
    JitInvokeInfo, StringFieldLayout,
};
use cratonvm_jit_api::JitRuntimeHelpers;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};

/// What the stubbed dispatch answers: no element pointer the tests build, and
/// no `size` they build either.
const MISS: i64 = 777;

thread_local! {
    /// Per test THREAD: the harness runs these tests in parallel, and the
    /// stub is called on the thread that runs the compiled code.
    static DISPATCHES: Cell<usize> = const { Cell::new(0) };
}

unsafe extern "C" fn counting_dispatch(
    _vm: i64,
    _info: *const JitInvokeInfo,
    _args: *const i64,
    _n: usize,
) -> i64 {
    DISPATCHES.with(|d| d.set(d.get() + 1));
    MISS
}

/// `java/util/ArrayList`'s class id in these tests. Small, because the compact
/// case registers a layout for it and `register_class_layout` indexes a dense
/// table by class id.
const AL_ID: u32 = 11;
/// Another plain class (a subclass, a `LinkedList`, anything).
const OTHER_ID: u32 = 12;
/// Field indices: `modCount` (inherited, an int) at 0, `elementData` at 1,
/// `size` at 2 -- the order `java/util/ArrayList` has.
const DATA_IDX: usize = 1;
const SIZE_IDX: usize = 2;

fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("r9w8 arraylist8 test invoked an unwired runtime helper");
    }
    let s = stub as *const () as usize;
    unsafe extern "C" fn record_throw_bci(_bci: i64) {}
    let throw_bci = record_throw_bci as *const () as usize;
    unsafe extern "C" fn deopt_unserviceable_stub(
        _vm: i64,
        _info: i64,
        _args: i64,
        _n: i64,
    ) -> i64 {
        i64::MIN
    }
    let deopt_unserviceable = deopt_unserviceable_stub as *const () as usize;
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
        self_call_stack_guard: 0,
        region_bounds_addr: 0,
        native_stack_floor_fn: 0,
        ldc_string: s,
        set_throw_bci: throw_bci,
        service_callee_deopt: deopt_unserviceable,
        ..Default::default()
    }
}

/// Register a compact layout for [`AL_ID`]: `elementData` (8-byte ref) at body
/// 0, `modCount` at 8, `size` at 12, 16-byte body. Idempotent.
fn register_compact_arraylist_layout() {
    use cratonvm_types::{register_class_layout, CompactLayout, FieldStorageKind};
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        register_class_layout(
            cratonvm_types::FIRST_LAYOUT_DOMAIN,
            AL_ID,
            std::sync::Arc::new(CompactLayout {
                field_offsets: vec![8, 0, 12],
                is_ref: vec![false, true, false],
                field_kinds: vec![
                    FieldStorageKind::Int,
                    FieldStorageKind::Reference,
                    FieldStorageKind::Int,
                ],
                ref_offsets: vec![0],
                body_size: 16,
            }),
        );
    });
}

/// The layout every test compiles against, built AFTER the compact layout is
/// registered so its compact offsets are the registered ones.
fn layout() -> StringFieldLayout {
    register_compact_arraylist_layout();
    let al = ArrayListFieldLayout::new(DATA_IDX, SIZE_IDX, AL_ID)
        .expect("the ArrayList layout must resolve in a default configuration");
    StringFieldLayout::new(0, Some(1), 2, 0x0055_0001).with_array_list(Some(al))
}

#[allow(clippy::too_many_arguments)]
fn compile(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    info: &JitInvokeInfo,
    invoke_pc: usize,
    string_layout: Option<StringFieldLayout>,
    method_key: &str,
    helpers: &JitRuntimeHelpers,
) -> CompiledMethod {
    compile_with_param_slots(
        // Not a door: hand-built bytecode with no method identity to admit.
        &cratonvm_jit::compile_gate::CompileAdmission::for_backend_test(),
        code,
        code_len,
        num_params,
        num_params, // max_locals
        true,       // needs_heap: the dispatch reads the VM pointer
        Vec::new(), // multianewarray_info
        Vec::new(), // field_info
        Vec::new(), // typecheck_info
        Vec::new(), // static_field_info
        Vec::new(), // new_info
        Vec::new(), // new_deferred_info
        Vec::new(), // anewarray_info
        Vec::new(), // anewarray_deferred_info
        vec![(invoke_pc, info as *const JitInvokeInfo)],
        Vec::new(),         // direct_calls
        Vec::new(),         // mic_slots
        Vec::new(),         // pic_slots
        Vec::new(),         // ldc_info
        Vec::new(),         // ldc_string_info
        Vec::new(),         // ldc_class_info
        Vec::new(),         // ldc2w_info
        Default::default(), // ldc_fp_pcs
        HashMap::new(),
        HashMap::new(),
        helpers,
        HashSet::new(),
        HashMap::new(),
        HashMap::new(), // inline_guard_variants
        string_layout,
        &[], // param_jvm_slots: identity
        0,   // param_slot_span
        0,   // param_oop_mask
        Vec::new(),
        method_key,
        None,       // despec
        Vec::new(), // indy_info
        None,       // elidable_init_pcs
        cratonvm_jit::x64::BackendRequest::default(),
    )
    .unwrap_or_else(|| panic!("JIT compilation of {method_key} failed"))
}

fn get_info() -> Box<JitInvokeInfo> {
    Box::new(JitInvokeInfo {
        class_name: "java/util/List",
        method_name: "get",
        descriptor: "(I)Ljava/lang/Object;",
        num_jit_args: 2,
        return_type: b'L',
        invoke_kind: 2,
        declaring_class_id: 0,
    })
}

fn size_info() -> Box<JitInvokeInfo> {
    Box::new(JitInvokeInfo {
        class_name: "java/util/ArrayList",
        method_name: "size",
        descriptor: "()I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 0,
        declaring_class_id: 0,
    })
}

/// `static Object f(List l, int i) { return l.get(i); }`
///   0: aload_0  1: iload_1  2: invokeinterface #1, 2  7: areturn
fn compile_get(info: &JitInvokeInfo, layout: Option<StringFieldLayout>) -> CompiledMethod {
    let code: Vec<u8> = vec![0x2a, 0x1b, 0xb9, 0x00, 0x01, 0x02, 0x00, 0xb0, 0, 0];
    compile(
        &code,
        8,
        2,
        info,
        2,
        layout,
        "R9W8Al.get:(Ljava/util/List;I)Ljava/lang/Object;",
        &helpers(),
    )
}

/// `static int f(ArrayList l) { return l.size(); }`
///   0: aload_0  1: invokevirtual #1  4: ireturn
fn compile_size(info: &JitInvokeInfo, layout: Option<StringFieldLayout>) -> CompiledMethod {
    let code: Vec<u8> = vec![0x2a, 0xb6, 0x00, 0x01, 0xac, 0, 0];
    compile(
        &code,
        5,
        1,
        info,
        1,
        layout,
        "R9W8Al.size:(Ljava/util/ArrayList;)I",
        &helpers(),
    )
}

/// A heap-shaped buffer: `words` 8-byte words, all zero.
struct Fake {
    words: Vec<u64>,
}

impl Fake {
    fn new(words: usize) -> Self {
        Fake {
            words: vec![0u64; words],
        }
    }
    fn ptr(&self) -> i64 {
        self.words.as_ptr() as i64 // Cast: object address as JIT argument
    }
    fn put_u32(&mut self, off: usize, v: u32) {
        let base = self.words.as_mut_ptr() as *mut u8; // Cast: byte view
        assert!(off + 4 <= self.words.len() * 8);
        // SAFETY: in bounds of the owned buffer, checked above.
        unsafe { std::ptr::copy_nonoverlapping(v.to_le_bytes().as_ptr(), base.add(off), 4) };
    }
    fn put_u64(&mut self, off: usize, v: u64) {
        let base = self.words.as_mut_ptr() as *mut u8; // Cast: byte view
        assert!(off + 8 <= self.words.len() * 8);
        // SAFETY: in bounds of the owned buffer, checked above.
        unsafe { std::ptr::copy_nonoverlapping(v.to_le_bytes().as_ptr(), base.add(off), 8) };
    }
    fn put_u8(&mut self, off: usize, v: u8) {
        assert!(off < self.words.len() * 8);
        let base = self.words.as_mut_ptr() as *mut u8; // Cast: byte view
                                                       // SAFETY: in bounds of the owned buffer, checked above.
        unsafe { *base.add(off) = v };
    }
}

const REF_ARRAY_TAGS: u8 = cratonvm_types::ObjectKind::Array as u8
    | ((cratonvm_types::ArrayElementType::Reference as u8) << 2);

/// A reference array of `len` slots holding `elems` (the rest null).
fn ref_array(len: usize, elems: &[u64]) -> Fake {
    let mut a = Fake::new(cratonvm_types::ARRAY_DATA_OFFSET / 8 + len.max(1));
    a.put_u32(cratonvm_types::ARRAY_LENGTH_OFFSET, len as u32); // Cast: small test length
    a.put_u8(cratonvm_types::KIND_TAGS_BYTE_OFFSET, REF_ARRAY_TAGS);
    for (i, &e) in elems.iter().enumerate() {
        a.put_u64(cratonvm_types::ARRAY_DATA_OFFSET + i * 8, e);
    }
    a
}

/// A LEGACY `ArrayList`-shaped instance: three 16-byte `Value` cells.
fn legacy_list(class_id: u32, data: i64, size: i32) -> Fake {
    let slot = cratonvm_types::SLOT_SIZE;
    let hdr = cratonvm_types::HEADER_SIZE;
    let mut o = Fake::new((hdr + 3 * slot) / 8);
    o.put_u32(0, class_id);
    o.put_u32(cratonvm_types::NUM_SLOTS_OFFSET, 3);
    // modCount: Int 0.
    // elementData: tag Object, pointer payload.
    let data_cell = hdr + DATA_IDX * slot;
    o.put_u32(data_cell, cratonvm_types::FIELD_CELL_TAG_OBJECT);
    o.put_u64(
        data_cell + cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET,
        data as u64,
    ); // Cast: pointer bits
       // size: tag Int (0), 4-byte payload.
    let size_cell = hdr + SIZE_IDX * slot;
    o.put_u32(
        size_cell + cratonvm_types::FIELD_CELL_PAYLOAD32_OFFSET,
        size as u32,
    ); // Cast: int bits
    o
}

/// A COMPACT instance of the registered layout: `elementData` at body 0,
/// `size` at body 12, `GC_FLAG_COMPACT` set.
fn compact_list(data: i64, size: i32) -> Fake {
    let hdr = cratonvm_types::HEADER_SIZE;
    let mut o = Fake::new((hdr + 16) / 8);
    o.put_u32(0, AL_ID);
    o.put_u32(cratonvm_types::NUM_SLOTS_OFFSET, 3);
    o.put_u8(
        cratonvm_types::GC_FLAGS_BYTE_OFFSET,
        cratonvm_types::GC_FLAG_COMPACT,
    );
    o.put_u64(hdr, data as u64); // Cast: pointer bits
    o.put_u32(hdr + 12, size as u32); // Cast: int bits
    o
}

/// Run `f(args)`; return (answer, dispatches made).
fn run(m: &CompiledMethod, args: &[i64]) -> (i64, usize) {
    let before = DISPATCHES.with(Cell::get);
    // SAFETY: JIT code compiled by this test from valid bytecode; every
    // argument is a live, aligned fake heap shape the prefix only reads, and
    // the only helper it can call is the counting dispatch above.
    let got = unsafe { m.try_call_with_context(0x1234_5678, args) }.expect("test JIT call");
    (got, DISPATCHES.with(Cell::get) - before)
}

fn inline_enabled() -> bool {
    !cratonvm_jit::arraylist_intrinsics_disabled()
        && !cratonvm_types::narrow_oop::narrow_oops_enabled()
        && cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_ALTRACE").is_none()
}

#[test]
fn get_hits_on_a_legacy_list_and_declines_everything_the_native_special_cases() {
    if !inline_enabled() {
        return;
    }
    let info = get_info();
    let m = compile_get(&info, Some(layout()));

    // Full backing array: length == size.
    let full = ref_array(3, &[0xA0, 0xB0, 0xC0]);
    let l = legacy_list(AL_ID, full.ptr(), 3);
    assert_eq!(run(&m, &[l.ptr(), 0]), (0xA0, 0), "get(0) is inline");
    assert_eq!(run(&m, &[l.ptr(), 2]), (0xC0, 0), "get(size-1) is inline");
    assert_eq!(
        run(&m, &[l.ptr(), 3]),
        (MISS, 1),
        "get(size) is the native's IOOBE"
    );
    assert_eq!(
        run(&m, &[l.ptr(), -1]),
        (MISS, 1),
        "a negative index declines"
    );

    // Spare capacity: length 5, size 3, the trailing slot null.
    let spare = ref_array(5, &[0xA0, 0xB0, 0xC0]);
    let l = legacy_list(AL_ID, spare.ptr(), 3);
    assert_eq!(run(&m, &[l.ptr(), 1]), (0xB0, 0));
    assert_eq!(
        run(&m, &[l.ptr(), 3]),
        (MISS, 1),
        "size <= i < length declines"
    );
    assert_eq!(run(&m, &[l.ptr(), 5]), (MISS, 1), "i == length declines");

    // The `values()`-view marker: a non-null trailing slot past `size`.
    let marked = ref_array(5, &[0xA0, 0xB0, 0xC0, 0, 0xEE]);
    let l = legacy_list(AL_ID, marked.ptr(), 3);
    assert_eq!(
        run(&m, &[l.ptr(), 0]),
        (MISS, 1),
        "a view carrier is the native's"
    );

    // Another class id: a subclass, a LinkedList, a view.
    let l = legacy_list(OTHER_ID, full.ptr(), 3);
    assert_eq!(
        run(&m, &[l.ptr(), 0]),
        (MISS, 1),
        "only EXACT ArrayList is answered"
    );

    // A null receiver: the dispatch reports the NPE.
    assert_eq!(run(&m, &[0, 0]), (MISS, 1), "null declines");

    // An array receiver carrying ArrayList's id as its component id.
    let mut arr = ref_array(3, &[1, 2, 3]);
    arr.put_u32(0, AL_ID);
    assert_eq!(
        run(&m, &[arr.ptr(), 0]),
        (MISS, 1),
        "an ArrayList[] is not an ArrayList"
    );

    // A null elementData.
    let l = legacy_list(AL_ID, 0, 0);
    assert_eq!(
        run(&m, &[l.ptr(), 0]),
        (MISS, 1),
        "null elementData declines"
    );

    // A primitive elementData.
    let mut prim = ref_array(3, &[1, 2, 3]);
    prim.put_u8(
        cratonvm_types::KIND_TAGS_BYTE_OFFSET,
        cratonvm_types::ObjectKind::Array as u8
            | ((cratonvm_types::ArrayElementType::Int as u8) << 2),
    );
    let l = legacy_list(AL_ID, prim.ptr(), 3);
    assert_eq!(
        run(&m, &[l.ptr(), 0]),
        (MISS, 1),
        "an int[] elementData declines"
    );

    // A negative size.
    let l = legacy_list(AL_ID, full.ptr(), -1);
    assert_eq!(
        run(&m, &[l.ptr(), 0]),
        (MISS, 1),
        "a negative size declines"
    );

    // A legacy `size` cell that does not say `Int`.
    let mut l = legacy_list(AL_ID, full.ptr(), 3);
    l.put_u32(
        cratonvm_types::HEADER_SIZE + SIZE_IDX * cratonvm_types::SLOT_SIZE,
        4,
    );
    assert_eq!(
        run(&m, &[l.ptr(), 0]),
        (MISS, 1),
        "a mistagged size cell declines"
    );

    // A legacy `elementData` cell that does not say `Object`.
    let mut l = legacy_list(AL_ID, full.ptr(), 3);
    l.put_u32(
        cratonvm_types::HEADER_SIZE + DATA_IDX * cratonvm_types::SLOT_SIZE,
        1,
    );
    assert_eq!(
        run(&m, &[l.ptr(), 0]),
        (MISS, 1),
        "a mistagged elementData cell declines"
    );

    // Too few slots for both cells.
    let mut l = legacy_list(AL_ID, full.ptr(), 3);
    l.put_u32(cratonvm_types::NUM_SLOTS_OFFSET, 2);
    assert_eq!(
        run(&m, &[l.ptr(), 0]),
        (MISS, 1),
        "a short instance declines"
    );
}

#[test]
fn get_and_size_hit_on_a_compact_list() {
    if !inline_enabled() || !cratonvm_types::compact_ref_fields_enabled() {
        return;
    }
    let get = get_info();
    let g = compile_get(&get, Some(layout()));
    let size = size_info();
    let s = compile_size(&size, Some(layout()));

    let spare = ref_array(4, &[0x10, 0x20]);
    let l = compact_list(spare.ptr(), 2);
    assert_eq!(run(&g, &[l.ptr(), 1]), (0x20, 0), "compact get is inline");
    assert_eq!(
        run(&g, &[l.ptr(), 2]),
        (MISS, 1),
        "compact get(size) declines"
    );
    assert_eq!(run(&s, &[l.ptr()]), (2, 0), "compact size is inline");
}

#[test]
fn size_hits_on_a_legacy_list_and_declines_like_the_native() {
    if !inline_enabled() {
        return;
    }
    let info = size_info();
    let m = compile_size(&info, Some(layout()));

    let spare = ref_array(10, &[1, 2, 3]);
    let l = legacy_list(AL_ID, spare.ptr(), 3);
    assert_eq!(run(&m, &[l.ptr()]), (3, 0), "size() is inline");

    let empty = ref_array(0, &[]);
    let l = legacy_list(AL_ID, empty.ptr(), 0);
    assert_eq!(run(&m, &[l.ptr()]), (0, 0), "an empty list is inline");

    let l = legacy_list(OTHER_ID, spare.ptr(), 3);
    assert_eq!(run(&m, &[l.ptr()]), (MISS, 1), "another class declines");

    let marked = ref_array(10, &[1, 2, 3, 0, 0, 0, 0, 0, 0, 9]);
    let l = legacy_list(AL_ID, marked.ptr(), 3);
    assert_eq!(
        run(&m, &[l.ptr()]),
        (MISS, 1),
        "a view carrier's size is the native's"
    );

    let l = legacy_list(AL_ID, 0, 3);
    assert_eq!(run(&m, &[l.ptr()]), (MISS, 1), "null elementData declines");
}

#[test]
fn without_an_arraylist_layout_every_call_dispatches() {
    let info = get_info();
    let plain = StringFieldLayout::new(0, Some(1), 2, 0x0055_0001);
    let m = compile_get(&info, Some(plain));
    let full = ref_array(3, &[0xA0, 0xB0, 0xC0]);
    let l = legacy_list(AL_ID, full.ptr(), 3);
    assert_eq!(run(&m, &[l.ptr(), 0]), (MISS, 1));
    let m = compile_get(&info, None);
    assert_eq!(run(&m, &[l.ptr(), 0]), (MISS, 1));
}

#[test]
fn the_resolver_answers_only_the_two_members_on_the_three_site_classes() {
    register_compact_arraylist_layout();
    let Some(al) = ArrayListFieldLayout::new(DATA_IDX, SIZE_IDX, AL_ID) else {
        return; // refused by configuration (kill switch, narrow oops, ALTRACE)
    };
    let get = "(I)Ljava/lang/Object;";
    for class in ["java/util/ArrayList", "java/util/List"] {
        assert_eq!(
            try_resolve_arraylist_intrinsic(class, "get", get, Some(al)),
            Some((JitIntrinsic::ArrayListGet.as_entry(), 1, b'L', AL_ID)),
            "{class}.get"
        );
    }
    for class in [
        "java/util/ArrayList",
        "java/util/List",
        "java/util/Collection",
    ] {
        assert_eq!(
            try_resolve_arraylist_intrinsic(class, "size", "()I", Some(al)),
            Some((JitIntrinsic::ArrayListSize.as_entry(), 0, b'I', AL_ID)),
            "{class}.size"
        );
    }
    for (class, name, desc) in [
        ("java/util/Collection", "get", get),
        ("java/util/Vector", "get", get),
        ("java/util/LinkedList", "size", "()I"),
        ("java/util/ArrayList", "isEmpty", "()Z"),
        ("java/util/ArrayList", "get", "(J)Ljava/lang/Object;"),
    ] {
        assert_eq!(
            try_resolve_arraylist_intrinsic(class, name, desc, Some(al)),
            None
        );
    }
    assert_eq!(
        try_resolve_arraylist_intrinsic("java/util/List", "get", get, None),
        None
    );
    assert!(JitIntrinsic::is_sentinel_entry(
        JitIntrinsic::ArrayListSize.as_entry()
    ));
}

#[test]
fn the_layout_refuses_a_zero_class_id_and_the_kill_switch() {
    assert!(ArrayListFieldLayout::new(DATA_IDX, SIZE_IDX, 0).is_none());
    assert!(ArrayListFieldLayout::new(DATA_IDX, DATA_IDX, AL_ID).is_none());
    let _off = cratonvm_types::flags::override_thread(
        cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
            "CRATONVM_NO_JIT_ARRAYLIST_INTRINSICS",
            Some("1"),
        )]),
    );
    assert!(cratonvm_jit::arraylist_intrinsics_disabled());
    assert!(ArrayListFieldLayout::new(DATA_IDX, SIZE_IDX, AL_ID).is_none());
}
