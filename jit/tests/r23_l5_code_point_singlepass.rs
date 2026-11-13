// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT r23 wave 1, lane L5 (INTRINSICS22-1): the single-pass
//! `String.codePointAt(int)` and `StringBuilder.appendCodePoint(int)` call-site
//! intrinsics (`jit/src/x64/string_code_point.rs`), EXECUTED.
//!
//! Each site is compiled the way both compile doors register it -- an
//! intrinsic `JitDirectCall` sentinel plus the site's own `JitInvokeInfo`
//! (`string_intrinsic_declines_to_a_call`) -- over hand-built LEGACY heap
//! shapes, and every answer is checked against a literal transcription of the
//! JDK 25 bodies whose UTF-16 pairing comes from Rust's own encoder. Every
//! input the JDK body throws on, grows on or inflates on must reach the
//! site's dispatch (stubbed here to count and answer [`MISS`]) with nothing
//! stored.

use cratonvm_jit::x64::compile_with_param_slots;
use cratonvm_jit::{
    try_resolve_string_intrinsic, CompiledMethod, JitDirectCall, JitInvokeInfo,
    StringBuilderFieldLayout, StringFieldLayout,
};
use cratonvm_jit_api::JitRuntimeHelpers;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};

/// What the stubbed dispatch answers.
const MISS: i64 = 777;

thread_local! {
    /// Per test THREAD: the stub runs on the thread that runs the code.
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

const STR_ID: u32 = 0x0023_5501;
const SB_ID: u32 = 0x0023_5502;
const OTHER_ID: u32 = 0x0023_5503;

fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("r23 L5 code-point test invoked an unwired runtime helper");
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

/// `value` at 0, `coder` at 1, `hash` at 2; the builder's `count` at 0,
/// `value` at 1, `coder` at 2. Neither class has a registered compact layout,
/// so every instance here is LEGACY (16-byte `Value` cells).
fn layout() -> Option<StringFieldLayout> {
    let b = StringBuilderFieldLayout::new(0, 1, 2, SB_ID)?;
    Some(StringFieldLayout::new(0, Some(1), 2, STR_ID).with_builder(Some(b)))
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
    fn bytes_mut(&mut self) -> &mut [u8] {
        let len = self.words.len() * 8;
        // SAFETY: the byte view of the owned buffer, same length.
        unsafe { std::slice::from_raw_parts_mut(self.words.as_mut_ptr() as *mut u8, len) }
    }
    fn bytes(&self) -> &[u8] {
        let len = self.words.len() * 8;
        // SAFETY: the byte view of the owned buffer, same length.
        unsafe { std::slice::from_raw_parts(self.words.as_ptr() as *const u8, len) }
    }
    fn put(&mut self, off: usize, v: &[u8]) {
        self.bytes_mut()[off..off + v.len()].copy_from_slice(v);
    }
    fn get_u32(&self, off: usize) -> u32 {
        let b = self.bytes();
        u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
    }
}

/// A `byte[]` of `data`, with `slack` zero bytes behind it the code must
/// never be allowed to read as elements.
fn byte_array(data: &[u8]) -> Fake {
    let mut a = Fake::new((cratonvm_types::ARRAY_DATA_OFFSET + data.len()).div_ceil(8) + 1);
    a.put(
        cratonvm_types::ARRAY_LENGTH_OFFSET,
        &(data.len() as u32).to_le_bytes(), // Cast: small test length
    );
    a.put(cratonvm_types::ARRAY_DATA_OFFSET, data);
    a
}

fn utf16_array(units: &[u16]) -> Fake {
    let raw: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
    byte_array(&raw)
}

/// A LEGACY String: `value` (a reference cell) and `coder` (an int cell).
fn string(l: &StringFieldLayout, value: i64, coder: u32, class_id: u32) -> Fake {
    let mut o = Fake::new((cratonvm_types::HEADER_SIZE + 3 * cratonvm_types::SLOT_SIZE) / 8);
    o.put(0, &class_id.to_le_bytes());
    o.put(l.value_legacy_offset as usize, &value.to_le_bytes());
    o.put(l.coder_legacy_offset as usize, &coder.to_le_bytes());
    o
}

/// A LEGACY StringBuilder: `count`, `value`, `coder`.
fn builder(
    b: &StringBuilderFieldLayout,
    value: i64,
    coder: u32,
    count: i32,
    class_id: u32,
) -> Fake {
    let mut o = Fake::new((cratonvm_types::HEADER_SIZE + 3 * cratonvm_types::SLOT_SIZE) / 8);
    o.put(0, &class_id.to_le_bytes());
    o.put(b.count_legacy_offset as usize, &count.to_le_bytes());
    o.put(b.value_legacy_offset as usize, &value.to_le_bytes());
    o.put(b.coder_legacy_offset as usize, &coder.to_le_bytes());
    o
}

/// `static <ret> f(<recv> r, int a) { return r.<method>(a); }`
///   0: aload_0  1: iload_1  2: invokevirtual #1  5: ireturn / areturn
fn compile_site(
    class: &'static str,
    method: &'static str,
    descriptor: &'static str,
    ret: u8,
    l: StringFieldLayout,
) -> Option<(CompiledMethod, Box<JitInvokeInfo>)> {
    compile_site_with(
        class,
        method,
        descriptor,
        ret,
        l,
        cratonvm_jit::x64::BackendRequest::default(),
    )
}

/// [`compile_site`] with the backend request of the caller (r23w1: the growth
/// test wires `DirectHelperTable::sb_grow`).
fn compile_site_with(
    class: &'static str,
    method: &'static str,
    descriptor: &'static str,
    ret: u8,
    l: StringFieldLayout,
    backend: cratonvm_jit::x64::BackendRequest,
) -> Option<(CompiledMethod, Box<JitInvokeInfo>)> {
    let (entry, num_params, ret_ty, guard_class_id) =
        try_resolve_string_intrinsic(class, method, descriptor, Some(l))?;
    let info = Box::new(JitInvokeInfo {
        class_name: class,
        method_name: method,
        descriptor,
        num_jit_args: num_params + 1,
        return_type: ret_ty,
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    });
    // `aload_0`, `iload_1` [, `iload_2`], `invokevirtual #1`, the return.
    let mut code: Vec<u8> = vec![0x2a, 0x1b];
    if num_params == 2 {
        code.push(0x1c);
    }
    let invoke_pc = code.len();
    code.extend([0xb6, 0x00, 0x01, ret]);
    let code_len = code.len();
    code.extend([0, 0]);
    let nargs = num_params + 1;
    let cm = compile_with_param_slots(
        // Not a door: hand-built bytecode with no method identity to admit.
        &cratonvm_jit::compile_gate::CompileAdmission::for_backend_test(),
        &code,
        code_len,
        nargs,
        nargs,      // max_locals
        true,       // needs_heap: the dispatch reads the VM pointer
        Vec::new(), // multianewarray_info
        Vec::new(), // field_info
        Vec::new(), // typecheck_info
        Vec::new(), // static_field_info
        Vec::new(), // new_info
        Vec::new(), // new_deferred_info
        Vec::new(), // anewarray_info
        Vec::new(), // anewarray_deferred_info
        vec![(invoke_pc, &*info as *const JitInvokeInfo)],
        vec![(
            invoke_pc,
            JitDirectCall {
                entry,
                needs_context: false,
                num_params,
                return_type: ret_ty,
                guard_class_id,
            },
        )],
        Vec::new(),         // mic_slots
        Vec::new(),         // pic_slots
        Vec::new(),         // ldc_info
        Vec::new(),         // ldc_string_info
        Vec::new(),         // ldc_class_info
        Vec::new(),         // ldc2w_info
        Default::default(), // ldc_fp_pcs
        HashMap::new(),
        HashMap::new(),
        &helpers(),
        HashSet::new(),
        HashMap::new(),
        HashMap::new(), // inline_guard_variants
        Some(l),
        &[], // param_jvm_slots: identity
        0,   // param_slot_span
        0,   // param_oop_mask
        Vec::new(),
        "R23L5.site:(Ljava/lang/Object;I)I",
        None,       // despec
        Vec::new(), // indy_info
        None,       // elidable_init_pcs
        backend,
    )
    .unwrap_or_else(|| panic!("JIT compilation of the {method} site failed"));
    Some((cm, info))
}

/// Run `f(r, a)`; return (answer, dispatches made).
fn run(m: &CompiledMethod, r: i64, a: i32) -> (i64, usize) {
    let before = DISPATCHES.with(Cell::get);
    // SAFETY: JIT code compiled by this test from valid bytecode; every
    // argument is a live, aligned fake heap shape or null, and the only helper
    // it can call is the counting dispatch above.
    let got =
        unsafe { m.try_call_with_context(0x1234_5678, &[r, i64::from(a)]) }.expect("test JIT call");
    (got, DISPATCHES.with(Cell::get) - before)
}

/// Run `f(r, a, c)`; return (answer, dispatches made).
fn run3(m: &CompiledMethod, r: i64, a: i32, c: i32) -> (i64, usize) {
    let before = DISPATCHES.with(Cell::get);
    // SAFETY: as [`run`].
    let got = unsafe { m.try_call_with_context(0x1234_5678, &[r, i64::from(a), i64::from(c)]) }
        .expect("test JIT call");
    (got, DISPATCHES.with(Cell::get) - before)
}

fn inline_enabled() -> bool {
    !cratonvm_types::narrow_oop::narrow_oops_enabled() && layout().is_some()
}

/// `String.codePointAt` / `StringUTF16.codePointAt` over `units`.
fn expect_code_point_at(units: &[u16], i: usize) -> i64 {
    let c1 = units[i];
    if (0xD800..0xDC00).contains(&c1) && i + 1 < units.len() {
        if let Some(Ok(c)) = char::decode_utf16([c1, units[i + 1]]).next() {
            return i64::from(u32::from(c));
        }
    }
    i64::from(c1)
}

#[test]
fn code_point_at_answers_inline_and_declines_every_throwing_index() {
    if !inline_enabled() {
        return;
    }
    let l = layout().expect("checked");
    let Some((m, _info)) = cratonvm_types::flags::with_thread_overrides(&[], || {
        compile_site("java/lang/String", "codePointAt", "(I)I", 0xac, l)
    }) else {
        return; // CRATONVM_JIT_SP_STRING_CODE_POINT_AT=0 in this environment
    };
    // LATIN1: the byte, zero-extended.
    let v = byte_array(&[b'c', 0xE9, 0xFF, 0x00]);
    let s = string(&l, v.ptr(), 0, STR_ID);
    for (i, want) in [(0, i64::from(b'c')), (1, 0xE9), (2, 0xFF), (3, 0)] {
        assert_eq!(run(&m, s.ptr(), i), (want, 0), "LATIN1 codePointAt({i})");
    }
    for bad in [4, 5, -1, i32::MIN, i32::MAX] {
        assert_eq!(
            run(&m, s.ptr(), bad),
            (MISS, 1),
            "LATIN1 codePointAt({bad}) calls"
        );
    }
    // UTF16: pairs, lone surrogates of both kinds, a reversed pair, a high
    // surrogate last, BMP units on both sides.
    let units: [u16; 11] = [
        0x0041, 0xD83D, 0xDE00, 0xDE00, 0xD800, 0x0042, 0xDBFF, 0xDFFF, 0xDC00, 0xD800, 0xD83D,
    ];
    let w = utf16_array(&units);
    let s16 = string(&l, w.ptr(), 1, STR_ID);
    for i in 0..units.len() {
        assert_eq!(
            run(&m, s16.ptr(), i as i32), // Cast: small test index
            (expect_code_point_at(&units, i), 0),
            "UTF16 codePointAt({i})"
        );
    }
    for bad in [11, 12, -1, i32::MIN] {
        assert_eq!(
            run(&m, s16.ptr(), bad),
            (MISS, 1),
            "UTF16 codePointAt({bad}) calls"
        );
    }
    // An odd trailing byte is not a unit.
    let odd = byte_array(&[0x3D, 0xD8, 0x00]);
    let s_odd = string(&l, odd.ptr(), 1, STR_ID);
    assert_eq!(
        run(&m, s_odd.ptr(), 0),
        (0xD83D, 0),
        "a lone high surrogate"
    );
    assert_eq!(run(&m, s_odd.ptr(), 1), (MISS, 1));
    // A null receiver, a null `value`, a coder that is no String's and a
    // receiver of another class all take the call.
    assert_eq!(run(&m, 0, 0), (MISS, 1), "null receiver");
    let no_value = string(&l, 0, 0, STR_ID);
    assert_eq!(run(&m, no_value.ptr(), 0), (MISS, 1), "null value");
    let bad_coder = string(&l, w.ptr(), 2, STR_ID);
    assert_eq!(run(&m, bad_coder.ptr(), 0), (MISS, 1), "coder 2");
    let other = string(&l, v.ptr(), 0, OTHER_ID);
    assert_eq!(run(&m, other.ptr(), 0), (MISS, 1), "another class id");
}

/// The JDK 25 `appendCodePoint` over a builder's `units`, when it fits
/// without growth or inflation; `None` when the JDK body would grow, inflate
/// or throw (the call's job).
fn expect_append(latin1: bool, capacity: usize, count: usize, cp: i32) -> Option<Vec<u16>> {
    if latin1 {
        return (0..=0xFF)
            .contains(&cp)
            .then(|| vec![cp as u16])
            .filter(|_| count < capacity); // Cast: checked range
    }
    let c = u32::try_from(cp).ok()?;
    if c >> 16 == 0 {
        return (count < capacity).then(|| vec![c as u16]); // Cast: a BMP code point
    }
    let ch = char::from_u32(c)?;
    let mut buf = [0u16; 2];
    let units = ch.encode_utf16(&mut buf).to_vec();
    (count + 2 <= capacity).then_some(units)
}

#[test]
fn append_code_point_appends_in_place_and_declines_growth_inflation_and_invalid() {
    if !inline_enabled() {
        return;
    }
    let l = layout().expect("checked");
    let b = l.builder.expect("checked");
    let Some((m, _info)) = cratonvm_types::flags::with_thread_overrides(&[], || {
        compile_site(
            "java/lang/StringBuilder",
            "appendCodePoint",
            "(I)Ljava/lang/StringBuilder;",
            0xb0,
            l,
        )
    }) else {
        return; // CRATONVM_JIT_SP_SB_APPEND_CODE_POINT=0 in this environment
    };
    let cps = [
        0x41,
        0xE9,
        0xFF,
        0x100,
        0x20AC,
        0xD800,
        0xDFFF,
        0xFFFF,
        0x1_0000,
        0x1_F600,
        0x10_FFFF,
        0x11_0000,
        -1,
        -5,
        i32::MIN,
        i32::MAX,
    ];
    let data = cratonvm_types::ARRAY_DATA_OFFSET;
    // LATIN1 builders of capacity 4.
    for count in [0usize, 1, 3, 4] {
        for &cp in &cps {
            let v = byte_array(&[b'x', b'y', b'z', b'w']);
            let sb = builder(&b, v.ptr(), 0, count as i32, SB_ID); // Cast: small count
            let before: Vec<u8> = v.bytes().to_vec();
            let got = run(&m, sb.ptr(), cp);
            match expect_append(true, 4, count, cp) {
                Some(units) => {
                    assert_eq!(got, (sb.ptr(), 0), "LATIN1 count={count} cp={cp:#x} inline");
                    assert_eq!(v.bytes()[data + count], units[0] as u8); // Cast: LATIN1 unit
                    assert_eq!(sb.get_u32(b.count_legacy_offset as usize), count as u32 + 1);
                }
                None => {
                    assert_eq!(got, (MISS, 1), "LATIN1 count={count} cp={cp:#x} calls");
                    assert_eq!(v.bytes(), &before[..], "nothing stored before a decline");
                    assert_eq!(sb.get_u32(b.count_legacy_offset as usize), count as u32);
                }
            }
        }
    }
    // UTF16 builders of capacity 4 chars.
    for count in [0usize, 1, 2, 3, 4] {
        for &cp in &cps {
            let v = utf16_array(&[0x61, 0x62, 0x63, 0x64]);
            let sb = builder(&b, v.ptr(), 1, count as i32, SB_ID); // Cast: small count
            let before: Vec<u8> = v.bytes().to_vec();
            let got = run(&m, sb.ptr(), cp);
            match expect_append(false, 4, count, cp) {
                Some(units) => {
                    assert_eq!(got, (sb.ptr(), 0), "UTF16 count={count} cp={cp:#x} inline");
                    for (k, &u) in units.iter().enumerate() {
                        let off = data + 2 * (count + k);
                        let stored = u16::from_le_bytes([v.bytes()[off], v.bytes()[off + 1]]);
                        assert_eq!(stored, u, "UTF16 count={count} cp={cp:#x} unit {k}");
                    }
                    assert_eq!(
                        sb.get_u32(b.count_legacy_offset as usize) as usize,
                        count + units.len()
                    );
                    // Nothing past the appended units.
                    let end = data + 2 * (count + units.len());
                    assert_eq!(&v.bytes()[end..], &before[end..]);
                }
                None => {
                    assert_eq!(got, (MISS, 1), "UTF16 count={count} cp={cp:#x} calls");
                    assert_eq!(v.bytes(), &before[..], "nothing stored before a decline");
                    assert_eq!(sb.get_u32(b.count_legacy_offset as usize) as usize, count);
                }
            }
        }
    }
    // A negative or oversized `count` never addresses the array (4 bytes: a
    // LATIN1 capacity of 4, a UTF16 one of 2).
    for coder in [0u32, 1] {
        for count in [-1i32, -2, i32::MIN, 4, 5, i32::MAX] {
            for cp in [0x41, 0x20AC, 0x1_F600] {
                let v = byte_array(&[0; 4]);
                let sb = builder(&b, v.ptr(), coder, count, SB_ID);
                let before: Vec<u8> = v.bytes().to_vec();
                assert_eq!(
                    run(&m, sb.ptr(), cp),
                    (MISS, 1),
                    "coder={coder} count={count} cp={cp:#x}"
                );
                assert_eq!(v.bytes(), &before[..]);
            }
        }
    }
    // A null receiver, a null payload, an unknown coder and another class
    // (a `StringBuffer`) all take the call.
    let v = byte_array(&[0; 4]);
    assert_eq!(run(&m, 0, 0x41), (MISS, 1), "null receiver");
    let no_value = builder(&b, 0, 0, 0, SB_ID);
    assert_eq!(run(&m, no_value.ptr(), 0x41), (MISS, 1), "null value");
    let bad_coder = builder(&b, v.ptr(), 2, 0, SB_ID);
    assert_eq!(run(&m, bad_coder.ptr(), 0x41), (MISS, 1), "coder 2");
    let other = builder(&b, v.ptr(), 0, 0, OTHER_ID);
    assert_eq!(run(&m, other.ptr(), 0x41), (MISS, 1), "another class id");
}

/// The JDK 25 answers over `units`: `codePointBefore(index)` and
/// `codePointCount(begin, end)`.
fn expect_before(units: &[u16], index: usize) -> i64 {
    let i = index - 1;
    let c2 = units[i];
    if (0xDC00..0xE000).contains(&c2) && i > 0 {
        if let Some(Ok(c)) = char::decode_utf16([units[i - 1], c2]).next() {
            if u32::from(c) > 0xFFFF {
                return i64::from(u32::from(c));
            }
        }
    }
    i64::from(c2)
}

fn expect_count(units: &[u16], begin: usize, end: usize) -> i64 {
    let mut n = (end - begin) as i64; // Cast: small test range
    let mut i = begin;
    while i < end {
        let c = units[i];
        i += 1;
        if (0xD800..0xDC00).contains(&c) && i < end && (0xDC00..0xE000).contains(&units[i]) {
            n -= 1;
            i += 1;
        }
    }
    n
}

const UNITS: [u16; 11] = [
    0x0041, 0xD83D, 0xDE00, 0xDE00, 0xD800, 0x0042, 0xDBFF, 0xDFFF, 0xDC00, 0xD800, 0xD83D,
];

#[test]
fn code_point_before_and_count_answer_inline_and_decline_every_throwing_argument() {
    if !inline_enabled() {
        return;
    }
    let l = layout().expect("checked");
    let compiled = cratonvm_types::flags::with_thread_overrides(&[], || {
        (
            compile_site("java/lang/String", "codePointBefore", "(I)I", 0xac, l),
            compile_site("java/lang/String", "codePointCount", "(II)I", 0xac, l),
        )
    });
    let (Some((before, _bi)), Some((count, _ci))) = compiled else {
        return; // CRATONVM_JIT_SP_STRING_CODE_POINT_BEFORE_COUNT=0 here
    };
    let v = byte_array(&[b'c', 0xE9, 0xFF, 0x00]);
    let s = string(&l, v.ptr(), 0, STR_ID);
    for (i, want) in [(1, i64::from(b'c')), (2, 0xE9), (3, 0xFF), (4, 0)] {
        assert_eq!(run(&before, s.ptr(), i), (want, 0), "LATIN1 before({i})");
    }
    for bad in [0, 5, -1, i32::MIN, i32::MAX] {
        assert_eq!(
            run(&before, s.ptr(), bad),
            (MISS, 1),
            "LATIN1 before({bad})"
        );
    }
    for (b, e) in [(0, 0), (0, 4), (1, 3), (4, 4)] {
        assert_eq!(
            run3(&count, s.ptr(), b, e),
            (i64::from(e - b), 0),
            "LATIN1 count"
        );
    }
    for (b, e) in [(-1, 2), (3, 1), (0, 5), (i32::MIN, 0), (0, i32::MAX)] {
        assert_eq!(
            run3(&count, s.ptr(), b, e),
            (MISS, 1),
            "LATIN1 count({b}, {e})"
        );
    }
    let w = utf16_array(&UNITS);
    let s16 = string(&l, w.ptr(), 1, STR_ID);
    for index in 1..=UNITS.len() {
        assert_eq!(
            run(&before, s16.ptr(), index as i32), // Cast: small test index
            (expect_before(&UNITS, index), 0),
            "UTF16 before({index})"
        );
    }
    for bad in [0, 12, -1] {
        assert_eq!(
            run(&before, s16.ptr(), bad),
            (MISS, 1),
            "UTF16 before({bad})"
        );
    }
    for b in 0..=UNITS.len() {
        for e in b..=UNITS.len() {
            assert_eq!(
                run3(&count, s16.ptr(), b as i32, e as i32), // Cast: small test indices
                (expect_count(&UNITS, b, e), 0),
                "UTF16 count({b}, {e})"
            );
        }
    }
    for (b, e) in [(-1, 2), (3, 1), (0, 12)] {
        assert_eq!(
            run3(&count, s16.ptr(), b, e),
            (MISS, 1),
            "UTF16 count({b}, {e})"
        );
    }
    let bad_coder = string(&l, w.ptr(), 2, STR_ID);
    assert_eq!(run(&before, bad_coder.ptr(), 1), (MISS, 1), "coder 2");
    assert_eq!(run3(&count, bad_coder.ptr(), 0, 1), (MISS, 1), "coder 2");
    assert_eq!(run(&before, 0, 1), (MISS, 1), "null receiver");
    assert_eq!(run3(&count, 0, 0, 0), (MISS, 1), "null receiver");
}

#[test]
fn builder_code_point_at_answers_inside_count_and_declines_the_rest() {
    if !inline_enabled() {
        return;
    }
    let l = layout().expect("checked");
    let b = l.builder.expect("checked");
    let Some((m, _info)) = cratonvm_types::flags::with_thread_overrides(&[], || {
        compile_site("java/lang/StringBuilder", "codePointAt", "(I)I", 0xac, l)
    }) else {
        return; // CRATONVM_JIT_SP_SB_CODE_POINT_AT=0 here
    };
    // LATIN1 builder: capacity 6, count 4.
    let v = byte_array(&[b'c', 0xE9, 0xFF, 0x00, b'x', b'y']);
    let sb = builder(&b, v.ptr(), 0, 4, SB_ID);
    for (i, want) in [(0, i64::from(b'c')), (1, 0xE9), (2, 0xFF), (3, 0)] {
        assert_eq!(run(&m, sb.ptr(), i), (want, 0), "LATIN1 codePointAt({i})");
    }
    for bad in [4, 5, 6, -1, i32::MIN] {
        assert_eq!(
            run(&m, sb.ptr(), bad),
            (MISS, 1),
            "LATIN1 codePointAt({bad})"
        );
    }
    // UTF16 builder at every count: a pair split by `count` answers its high
    // half (`index + 1 < count` fails).
    let w = utf16_array(&UNITS);
    for count in 0..=UNITS.len() {
        let sb16 = builder(&b, w.ptr(), 1, count as i32, SB_ID); // Cast: small count
        for i in 0..count {
            assert_eq!(
                run(&m, sb16.ptr(), i as i32), // Cast: small test index
                (expect_code_point_at(&UNITS[..count], i), 0),
                "UTF16 count={count} codePointAt({i})"
            );
        }
        for bad in [count as i32, -1] {
            assert_eq!(
                run(&m, sb16.ptr(), bad),
                (MISS, 1),
                "UTF16 count={count} at {bad}"
            );
        }
    }
    // A count past the payload declines (the JDK checks the payload too), and
    // so do a coder 2, a null payload, a null receiver and another class.
    let short = utf16_array(&[0xD83D]);
    let over = builder(&b, short.ptr(), 1, 3, SB_ID);
    assert_eq!(
        run(&m, over.ptr(), 0),
        (MISS, 1),
        "pair partner past the payload"
    );
    assert_eq!(run(&m, over.ptr(), 1), (MISS, 1), "index past the payload");
    let coder2 = builder(&b, w.ptr(), 2, 3, SB_ID);
    assert_eq!(run(&m, coder2.ptr(), 0), (MISS, 1));
    let no_value = builder(&b, 0, 1, 3, SB_ID);
    assert_eq!(run(&m, no_value.ptr(), 0), (MISS, 1));
    assert_eq!(run(&m, 0, 0), (MISS, 1));
    let other = builder(&b, w.ptr(), 1, 3, OTHER_ID);
    assert_eq!(run(&m, other.ptr(), 0), (MISS, 1));
}

// ---- tier2 wave 2, page tag `intr22` (INTRINSICS22-1 review) --------------
//
// Every test above builds LEGACY instances only (16-byte `Value` cells), but a
// production VM with compact fields hands the same sites COMPACT instances
// (`GC_FLAG_COMPACT` in the header, packed field payloads, a one-byte
// `coder`), and each body is emitted twice, once per layout. The tests below
// compile the SAME site against a layout whose compact offsets differ from the
// legacy ones and drive both instance shapes through ONE compiled method, so
// the compact arm (and the one-byte `coder` load) is executed, not only
// emitted. Expected values are the JDK 25 bodies' (the `expect_*`
// transcriptions above). Derived, not measured.

/// Compact payload offsets, clear of the header (class id 0..4, mark word
/// 4..8 with the GC flags byte at 7) and of each other.
const INTR22_COUNT: usize = 16;
const INTR22_SB_CODER: usize = 20;
const INTR22_SB_VALUE: usize = 24;
const INTR22_STR_VALUE: usize = 16;
const INTR22_STR_CODER: usize = 24;

/// [`layout`] with COMPACT offsets set (and a one-byte `coder` in both
/// classes), the legacy offsets untouched.
fn intr22_compact_layout() -> Option<StringFieldLayout> {
    let mut b = layout()?.builder?;
    b.count_compact_offset = INTR22_COUNT as i32; // Cast: small constant
    b.coder_compact_offset = INTR22_SB_CODER as i32; // Cast: small constant
    b.coder_compact_is_byte = true;
    b.value_compact_offset = INTR22_SB_VALUE as i32; // Cast: small constant
    let mut l = layout()?;
    l.value_compact_offset = INTR22_STR_VALUE as i32; // Cast: small constant
    l.coder_compact_offset = INTR22_STR_CODER as i32; // Cast: small constant
    l.coder_compact_is_byte = true;
    Some(l.with_builder(Some(b)))
}

/// A COMPACT String: the flag set, `value` at its compact offset, a one-byte
/// `coder`. The legacy cells stay zero, so a body that read them would see a
/// null `value`.
fn intr22_compact_string(value: i64, coder: u8, class_id: u32) -> Fake {
    let mut o = Fake::new(6);
    o.put(0, &class_id.to_le_bytes());
    o.put(
        cratonvm_types::GC_FLAGS_BYTE_OFFSET,
        &[cratonvm_types::GC_FLAG_COMPACT],
    );
    o.put(INTR22_STR_VALUE, &value.to_le_bytes());
    o.put(INTR22_STR_CODER, &[coder]);
    o
}

/// A COMPACT StringBuilder: the flag set, `count` (4 bytes), a one-byte
/// `coder`, `value`.
fn intr22_compact_builder(value: i64, coder: u8, count: i32, class_id: u32) -> Fake {
    let mut o = Fake::new(6);
    o.put(0, &class_id.to_le_bytes());
    o.put(
        cratonvm_types::GC_FLAGS_BYTE_OFFSET,
        &[cratonvm_types::GC_FLAG_COMPACT],
    );
    o.put(INTR22_COUNT, &count.to_le_bytes());
    o.put(INTR22_SB_CODER, &[coder]);
    o.put(INTR22_SB_VALUE, &value.to_le_bytes());
    o
}

#[test]
fn intr22_compact_instances_answer_code_point_at_like_legacy_ones() {
    if !inline_enabled() {
        return;
    }
    let Some(l) = intr22_compact_layout() else {
        return;
    };
    let Some((m, _info)) = cratonvm_types::flags::with_thread_overrides(&[], || {
        compile_site("java/lang/String", "codePointAt", "(I)I", 0xac, l)
    }) else {
        return; // CRATONVM_JIT_SP_STRING_CODE_POINT_AT=0 in this environment
    };
    // LATIN1, compact then legacy through the one compiled method.
    let v = byte_array(&[b'c', 0xE9, 0xFF]);
    let compact = intr22_compact_string(v.ptr(), 0, STR_ID);
    let legacy = string(&l, v.ptr(), 0, STR_ID);
    for s in [&compact, &legacy] {
        for (i, want) in [(0, i64::from(b'c')), (1, 0xE9), (2, 0xFF)] {
            assert_eq!(run(&m, s.ptr(), i), (want, 0), "LATIN1 codePointAt({i})");
        }
        for bad in [3, -1] {
            assert_eq!(
                run(&m, s.ptr(), bad),
                (MISS, 1),
                "LATIN1 codePointAt({bad})"
            );
        }
    }
    // UTF16, every index.
    let w = utf16_array(&UNITS);
    let compact16 = intr22_compact_string(w.ptr(), 1, STR_ID);
    let legacy16 = string(&l, w.ptr(), 1, STR_ID);
    for s in [&compact16, &legacy16] {
        for i in 0..UNITS.len() {
            assert_eq!(
                run(&m, s.ptr(), i as i32), // Cast: small test index
                (expect_code_point_at(&UNITS, i), 0),
                "UTF16 codePointAt({i})"
            );
        }
        let past = UNITS.len() as i32; // Cast: small length
        assert_eq!(run(&m, s.ptr(), past), (MISS, 1));
    }
    // The compact `coder` is ONE byte: stray non-zero bytes right behind it
    // must not turn a LATIN1 string into a UTF16 one.
    let mut noisy = intr22_compact_string(v.ptr(), 0, STR_ID);
    noisy.put(INTR22_STR_CODER + 1, &[0xFF, 0xFF, 0xFF]);
    assert_eq!(run(&m, noisy.ptr(), 1), (0xE9, 0), "a one-byte coder");
    // A null compact `value` and an unknown compact coder decline.
    let no_value = intr22_compact_string(0, 0, STR_ID);
    assert_eq!(run(&m, no_value.ptr(), 0), (MISS, 1), "null compact value");
    let bad_coder = intr22_compact_string(w.ptr(), 2, STR_ID);
    assert_eq!(run(&m, bad_coder.ptr(), 0), (MISS, 1), "compact coder 2");
}

#[test]
fn intr22_compact_instances_append_code_point_like_legacy_ones() {
    if !inline_enabled() {
        return;
    }
    let Some(l) = intr22_compact_layout() else {
        return;
    };
    let Some((m, _info)) = cratonvm_types::flags::with_thread_overrides(&[], || {
        compile_site(
            "java/lang/StringBuilder",
            "appendCodePoint",
            "(I)Ljava/lang/StringBuilder;",
            0xb0,
            l,
        )
    }) else {
        return; // CRATONVM_JIT_SP_SB_APPEND_CODE_POINT=0 in this environment
    };
    let cps = [
        0x41, 0xFF, 0x100, 0x20AC, 0xD800, 0x1_0000, 0x1_F600, 0x11_0000, -1,
    ];
    let data = cratonvm_types::ARRAY_DATA_OFFSET;
    // LATIN1 and UTF16 compact builders of capacity 4 chars, every count.
    for (coder, latin1) in [(0u8, true), (1u8, false)] {
        for count in 0..=4usize {
            for &cp in &cps {
                let v = if latin1 {
                    byte_array(&[b'x', b'y', b'z', b'w'])
                } else {
                    utf16_array(&[0x61, 0x62, 0x63, 0x64])
                };
                let sb = intr22_compact_builder(v.ptr(), coder, count as i32, SB_ID); // Cast: small
                let before: Vec<u8> = v.bytes().to_vec();
                let got = run(&m, sb.ptr(), cp);
                let what = format!("coder={coder} count={count} cp={cp:#x}");
                match expect_append(latin1, 4, count, cp) {
                    Some(units) => {
                        assert_eq!(got, (sb.ptr(), 0), "{what} inline");
                        for (k, &u) in units.iter().enumerate() {
                            if latin1 {
                                // Cast: a LATIN1 unit fits a byte.
                                assert_eq!(v.bytes()[data + count + k], u as u8, "{what}");
                            } else {
                                let off = data + 2 * (count + k);
                                let stored =
                                    u16::from_le_bytes([v.bytes()[off], v.bytes()[off + 1]]);
                                assert_eq!(stored, u, "{what} unit {k}");
                            }
                        }
                        assert_eq!(
                            sb.get_u32(INTR22_COUNT) as usize,
                            count + units.len(),
                            "{what}"
                        );
                        // The compact `coder` byte is untouched.
                        assert_eq!(sb.bytes()[INTR22_SB_CODER], coder, "{what}");
                    }
                    None => {
                        assert_eq!(got, (MISS, 1), "{what} calls");
                        assert_eq!(v.bytes(), &before[..], "{what}: nothing stored");
                        assert_eq!(sb.get_u32(INTR22_COUNT) as usize, count, "{what}");
                    }
                }
            }
        }
    }
    // A legacy builder through the same compiled site takes the legacy arm.
    let b = l.builder.expect("checked");
    let v = byte_array(&[0; 4]);
    let legacy = builder(&b, v.ptr(), 0, 1, SB_ID);
    assert_eq!(run(&m, legacy.ptr(), 0x41), (legacy.ptr(), 0));
    assert_eq!(legacy.get_u32(b.count_legacy_offset as usize), 2);
    assert_eq!(v.bytes()[data + 1], 0x41);
    // A null compact payload, an unknown compact coder and another class decline.
    let w = byte_array(&[0; 4]);
    let none = intr22_compact_builder(0, 0, 0, SB_ID);
    assert_eq!(run(&m, none.ptr(), 0x41), (MISS, 1), "null compact value");
    let bad_coder = intr22_compact_builder(w.ptr(), 2, 0, SB_ID);
    assert_eq!(run(&m, bad_coder.ptr(), 0x41), (MISS, 1), "compact coder 2");
    let other = intr22_compact_builder(w.ptr(), 0, 0, OTHER_ID);
    assert_eq!(run(&m, other.ptr(), 0x41), (MISS, 1), "another class id");
}

#[test]
fn intr22_compact_instances_builder_code_point_at_like_legacy_ones() {
    if !inline_enabled() {
        return;
    }
    let Some(l) = intr22_compact_layout() else {
        return;
    };
    let Some((m, _info)) = cratonvm_types::flags::with_thread_overrides(&[], || {
        compile_site("java/lang/StringBuilder", "codePointAt", "(I)I", 0xac, l)
    }) else {
        return; // CRATONVM_JIT_SP_SB_CODE_POINT_AT=0 in this environment
    };
    let v = byte_array(&[b'c', 0xE9, 0xFF, 0x00, b'x', b'y']);
    let sb = intr22_compact_builder(v.ptr(), 0, 4, SB_ID);
    for (i, want) in [(0, i64::from(b'c')), (1, 0xE9), (2, 0xFF), (3, 0)] {
        assert_eq!(run(&m, sb.ptr(), i), (want, 0), "LATIN1 codePointAt({i})");
    }
    for bad in [4, 5, -1] {
        assert_eq!(
            run(&m, sb.ptr(), bad),
            (MISS, 1),
            "LATIN1 codePointAt({bad})"
        );
    }
    let w = utf16_array(&UNITS);
    for count in 0..=UNITS.len() {
        let sb16 = intr22_compact_builder(w.ptr(), 1, count as i32, SB_ID); // Cast: small count
        for i in 0..count {
            assert_eq!(
                run(&m, sb16.ptr(), i as i32), // Cast: small test index
                (expect_code_point_at(&UNITS[..count], i), 0),
                "UTF16 count={count} codePointAt({i})"
            );
        }
        assert_eq!(run(&m, sb16.ptr(), count as i32), (MISS, 1)); // Cast: small count
    }
}

// ---- r23w1 (R23W1L5-1): the growth stub of the single-pass `appendCodePoint` ----
//
// `emit_sb_grow_stub` is shared with `append(char)` / `append(String)` (str17)
// and no test executed it. The test below wires `DirectHelperTable::sb_grow` to
// a Rust double that installs a larger `byte[]` in the fake builder, then
// checks the retry: the code point is stored in the grown payload, `count`
// moves, the receiver comes back, and NO dispatch was made. Every shape the stub
// must not serve (inflation, an invalid code point, a negative `count`, a UTF16
// payload, a helper that declines) reaches the dispatch with nothing stored.
// Expected values are the JDK 25 `append(char)` LATIN1 arm after
// `ensureCapacityInternal`. Derived, not measured.
//
// Fixture bounds: the arrays are `byte_array`s whose length and data offsets
// are the real `ARRAY_LENGTH_OFFSET` / `ARRAY_DATA_OFFSET`; the double reads
// the old payload only inside its own `length`, and writes only the builder's
// `value` cell and a freshly allocated array. No class-id-indexed table is
// involved (`SB_ID` is an immediate compared with the header).

thread_local! {
    static GROWS: Cell<usize> = const { Cell::new(0) };
    static GROW_SERVES: Cell<bool> = const { Cell::new(true) };
    static GROW_MIN: Cell<i64> = const { Cell::new(-1) };
    static GROWN: std::cell::RefCell<Vec<Fake>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// `DirectHelperTable::sb_grow`: `(vm, builder, value_offset, min_capacity) ->
/// builder | 0`. Doubles (or grows to `min_capacity`), copies the old payload.
unsafe extern "C" fn fake_sb_grow(
    _vm: i64,
    builder: i64,
    value_offset: i64,
    min_capacity: i64,
) -> i64 {
    GROWS.with(|g| g.set(g.get() + 1));
    GROW_MIN.with(|m| m.set(min_capacity));
    if !GROW_SERVES.with(Cell::get) {
        return 0;
    }
    let slot = (builder + value_offset) as *mut i64;
    // SAFETY: `builder` is the live fake builder the compiled code passed and
    // `value_offset` is its `value` cell, which holds a fake `byte[]` pointer
    // whose `length` field is at `ARRAY_LENGTH_OFFSET`.
    let (old, old_len) = unsafe {
        let old = std::ptr::read_unaligned(slot);
        let len = std::ptr::read_unaligned(
            (old + cratonvm_types::ARRAY_LENGTH_OFFSET as i64) as *const u32,
        );
        (old, len as usize)
    };
    let want = usize::try_from(min_capacity).unwrap_or(0).max(old_len * 2);
    let mut data = vec![0u8; want];
    // SAFETY: the old payload has `old_len` bytes at `ARRAY_DATA_OFFSET`, and
    // `data` has `want >= old_len` bytes of its own.
    unsafe {
        std::ptr::copy_nonoverlapping(
            (old + cratonvm_types::ARRAY_DATA_OFFSET as i64) as *const u8,
            data.as_mut_ptr(),
            old_len.min(want),
        );
    }
    let grown = byte_array(&data);
    let grown_ptr = grown.ptr();
    // The heap buffer of a `Fake` does not move when the `Fake` is pushed.
    GROWN.with(|g| g.borrow_mut().push(grown));
    // SAFETY: as the read above.
    unsafe { std::ptr::write_unaligned(slot, grown_ptr) };
    builder
}

fn reset_grow_double(serves: bool) {
    GROWS.with(|g| g.set(0));
    GROW_MIN.with(|m| m.set(-1));
    GROW_SERVES.with(|s| s.set(serves));
    GROWN.with(|g| g.borrow_mut().clear());
}

/// The payload bytes (element data only) of the array the double installed last.
fn last_grown_payload() -> Option<Vec<u8>> {
    GROWN.with(|g| {
        g.borrow()
            .last()
            .map(|a| a.bytes()[cratonvm_types::ARRAY_DATA_OFFSET..].to_vec())
    })
}

#[test]
fn r23w1_append_code_point_grows_a_full_latin1_payload_and_declines_the_rest() {
    if !inline_enabled()
        || !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_SB_GROW")
        || !cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_JIT_SP_SB_APPEND_CODE_POINT_GROW",
        )
    {
        return;
    }
    let l = layout().expect("checked");
    let b = l.builder.expect("checked");
    let backend = cratonvm_jit::x64::BackendRequest {
        direct_helpers: cratonvm_jit::DirectHelperTable {
            sb_grow: fake_sb_grow as *const () as usize,
            ..cratonvm_jit::DirectHelperTable::EMPTY
        },
        ..cratonvm_jit::x64::BackendRequest::default()
    };
    let Some((m, _info)) = cratonvm_types::flags::with_thread_overrides(&[], || {
        compile_site_with(
            "java/lang/StringBuilder",
            "appendCodePoint",
            "(I)Ljava/lang/StringBuilder;",
            0xb0,
            l,
            backend,
        )
    }) else {
        return; // CRATONVM_JIT_SP_SB_APPEND_CODE_POINT=0 in this environment
    };
    let data = cratonvm_types::ARRAY_DATA_OFFSET;
    let count_at = b.count_legacy_offset as usize;

    // Served: a full LATIN1 payload of 4 grows to 8 (min capacity 5), the old
    // bytes survive, the code point lands at index 4, and nothing is dispatched.
    for cp in [0x41, 0xE9, 0xFF] {
        reset_grow_double(true);
        let v = byte_array(&[b'x', b'y', b'z', b'w']);
        let sb = builder(&b, v.ptr(), 0, 4, SB_ID);
        assert_eq!(run(&m, sb.ptr(), cp), (sb.ptr(), 0), "grown append cp={cp:#x}");
        assert_eq!(GROWS.with(Cell::get), 1, "one growth, cp={cp:#x}");
        assert_eq!(GROW_MIN.with(Cell::get), 5, "min capacity is count + 1");
        let grown = last_grown_payload().expect("the double installed a payload");
        // Cast: a LATIN1 unit.
        assert_eq!(&grown[..5], &[b'x', b'y', b'z', b'w', cp as u8], "cp={cp:#x}");
        assert_eq!(sb.get_u32(count_at), 5, "count moved to the grown payload");
        assert_eq!(&v.bytes()[data..data + 4], b"xyzw", "the old payload is untouched");
    }
    // Served from an EMPTY payload (length 0, count 0).
    reset_grow_double(true);
    let empty = byte_array(&[]);
    let sb = builder(&b, empty.ptr(), 0, 0, SB_ID);
    assert_eq!(run(&m, sb.ptr(), 0x61), (sb.ptr(), 0), "empty payload");
    assert_eq!(GROW_MIN.with(Cell::get), 1);
    assert_eq!(last_grown_payload().expect("grown")[0], 0x61);
    assert_eq!(sb.get_u32(count_at), 1);

    // Not full: inline, no growth.
    reset_grow_double(true);
    let v = byte_array(&[0; 4]);
    let sb = builder(&b, v.ptr(), 0, 3, SB_ID);
    assert_eq!(run(&m, sb.ptr(), 0x42), (sb.ptr(), 0));
    assert_eq!(GROWS.with(Cell::get), 0, "room: no growth");
    assert_eq!(v.bytes()[data + 3], 0x42);

    // Full, but the code point inflates the builder or is invalid: the call,
    // and the stub is not even entered.
    for cp in [0x100, 0x20AC, 0x1_F600, 0x11_0000, -1, i32::MIN] {
        reset_grow_double(true);
        let v = byte_array(&[b'x', b'y', b'z', b'w']);
        let sb = builder(&b, v.ptr(), 0, 4, SB_ID);
        assert_eq!(run(&m, sb.ptr(), cp), (MISS, 1), "full, cp={cp:#x} calls");
        assert_eq!(GROWS.with(Cell::get), 0, "no growth for cp={cp:#x}");
        assert_eq!(sb.get_u32(count_at), 4);
        assert_eq!(&v.bytes()[data..data + 4], b"xyzw");
    }

    // A negative `count` reads as full but is never grown.
    for count in [-1i32, i32::MIN] {
        reset_grow_double(true);
        let v = byte_array(&[b'x', b'y', b'z', b'w']);
        let sb = builder(&b, v.ptr(), 0, count, SB_ID);
        assert_eq!(run(&m, sb.ptr(), 0x41), (MISS, 1), "count={count} calls");
        assert_eq!(GROWS.with(Cell::get), 0, "count={count} is not grown");
        assert_eq!(&v.bytes()[data..data + 4], b"xyzw");
    }

    // The helper declines (returns 0): one growth attempt, then the call, with
    // nothing stored and the operands intact.
    reset_grow_double(false);
    let v = byte_array(&[b'x', b'y', b'z', b'w']);
    let sb = builder(&b, v.ptr(), 0, 4, SB_ID);
    assert_eq!(run(&m, sb.ptr(), 0x41), (MISS, 1), "a declined growth calls");
    assert_eq!(GROWS.with(Cell::get), 1);
    assert_eq!(sb.get_u32(count_at), 4);
    assert_eq!(&v.bytes()[data..data + 4], b"xyzw");

    // A UTF16 payload is never grown by this arm (the helper serves LATIN1 only).
    reset_grow_double(true);
    let w = utf16_array(&[0x61, 0x62]);
    let sb16 = builder(&b, w.ptr(), 1, 2, SB_ID);
    assert_eq!(run(&m, sb16.ptr(), 0x41), (MISS, 1), "full UTF16 calls");
    assert_eq!(GROWS.with(Cell::get), 0);
}
