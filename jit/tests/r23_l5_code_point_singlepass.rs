// SPDX-License-Identifier: Apache-2.0
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
        cratonvm_jit::x64::BackendRequest::default(),
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
