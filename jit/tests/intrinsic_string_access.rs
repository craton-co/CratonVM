// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Differential tests for the STRING_ACCESS JIT intrinsic family — the
//! `java/lang/String` instance-method call-site intrinsics (Phase 3a):
//! `length()I`, `isEmpty()Z`, `charAt(I)C`, `hashCode()I`.
//!
//! The foundation waves (commits 06bfac0 / 544cbea) added inline `getfield`
//! and inline array-access codegen plus the `StringFieldLayout` API, so these
//! four methods are now inlined as machine code with NO `CALL` on the fast
//! path. They are registered ONLY when a `StringFieldLayout` (carrying a
//! `coder` field) is available — `cratonvm_jit::try_resolve_string_intrinsic`
//! takes the layout as a parameter; the layout-free `try_resolve_intrinsic`
//! deliberately never registers a String method, so a bare 3-arg matcher call
//! safely falls back to native dispatch.
//!
//! Each test builds a real heap `java/lang/String` object byte-for-byte like
//! the VM's compact layout — a 40-byte `ObjectHeader` followed by three
//! 16-byte `Value` field cells (`value` ref, `coder` int, `hash` int) — with
//! a `byte[]` backing array, JIT-compiles a one-line wrapper that invokes the
//! method, runs it, and asserts the result against a host-computed reference
//! that mirrors `native-builtins/src/lang_string.rs`.
//!
//! `length()`/`isEmpty()` compute `value.length >> coder`; `charAt` decodes a
//! LATIN1 byte (zero-extend) or a UTF-16 little-endian byte pair; `hashCode`
//! runs the `h = 31*h + c` polynomial. Out-of-bounds `charAt` indices and
//! null receivers branch to the deopt stub.

use cratonvm_jit::x64::compile;
use cratonvm_jit::{try_resolve_string_intrinsic, JitDirectCall, StringFieldLayout};
use cratonvm_jit_api::JitRuntimeHelpers;
use cratonvm_types::{ArrayElementType, ClassId, ObjectHeader, ObjectKind, HEADER_SIZE, SLOT_SIZE};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

/// Counts legacy `uncommon_trap` invocations. With default real-frame deopt,
/// failed guards stash a reconstructed frame instead, so the test helpers below
/// accept either signal while still verifying that the inline guard bailed.
static TRAP_COUNT: AtomicU64 = AtomicU64::new(0);
static DEOPT_LOCK: Mutex<()> = Mutex::new(());

unsafe extern "C" fn recording_uncommon_trap(_vm: i64, _reason: i64, _bci: i64) -> i64 {
    TRAP_COUNT.fetch_add(1, Ordering::SeqCst);
    0 // deopt action code; the JIT stub then returns i64::MIN itself
}

fn deopt_lock() -> MutexGuard<'static, ()> {
    DEOPT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn clear_deopt_signals() -> u64 {
    let _ = cratonvm_jit::deopt::take_last_deopt();
    TRAP_COUNT.load(Ordering::SeqCst)
}

fn assert_one_deopt_after(before_traps: u64, context: &str) {
    let trap_delta = TRAP_COUNT
        .load(Ordering::SeqCst)
        .saturating_sub(before_traps);
    let frame_deopt = cratonvm_jit::deopt::take_last_deopt();
    let signal_count = trap_delta + u64::from(frame_deopt.is_some());
    assert_eq!(
        signal_count,
        1,
        "{context} must trigger exactly one deopt; legacy_traps={trap_delta}, real_frame_deopt={}",
        frame_deopt.is_some()
    );
}

fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("STRING_ACCESS intrinsic test invoked an unwired runtime helper");
    }
    let s = stub as *const () as usize;
    // `set_throw_bci` only records the throwing bci in a thread-local; the
    // backend calls it on the throw path of EVERY method that has an exception
    // check, so reaching it is normal rather than a sign of missing wiring.
    // Give it a real no-op instead of the panicking stub.
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
        invoke_dispatch: s,
        invoke_virtual_mic: s,
        lambda_int_to_double: s,
        write_barrier: s,
        satb_pre_write_barrier: s,
        uncommon_trap: recording_uncommon_trap as *const () as usize,
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
        // Reached through emit_call_absolute, so a 0 here is a null CALL
        // (SIGSEGV), not an inert "unwired" sentinel. Neither is a sign of
        // missing wiring: `set_throw_bci` just records the throwing bci, and
        // `service_callee_deopt` is the normal IR direct-call path when a
        // callee returns the i64::MIN "threw" sentinel. The deopt stub returns
        // that sentinel unchanged -- exactly what the real helper does for a
        // vm/info it cannot service -- so the caller propagates the throw.
        set_throw_bci: throw_bci,
        service_callee_deopt: deopt_unserviceable,
        ..Default::default()
    }
}

/// An arbitrary non-zero class id stamped into every fake String header.
const STRING_CLASS_ID: u32 = 0x5712_3400;

/// The compact `java/lang/String` field layout used by every test:
/// `value` at field index 0, `coder` at 1, `hash` at 2, guard class id
/// = `STRING_CLASS_ID` (matches the id `make_string` stamps at `[recv+0]`).
fn string_layout() -> StringFieldLayout {
    StringFieldLayout::new(0, Some(1), 2, STRING_CLASS_ID)
}

/// A heap object: a `HEADER_SIZE`-byte `ObjectHeader` followed by tightly
/// packed element data (for arrays) or 16-byte `Value` field cells (for
/// instances). Backed by a `Vec<u64>` so the base address is 8-byte aligned.
struct FakeObj {
    storage: Vec<u64>,
}

impl FakeObj {
    fn with_bytes(total: usize) -> Self {
        FakeObj {
            storage: vec![0u64; total.div_ceil(8).max(1)],
        }
    }
    fn base(&mut self) -> *mut u8 {
        self.storage.as_mut_ptr() as *mut u8
    }
    fn ptr(&self) -> i64 {
        self.storage.as_ptr() as i64
    }
}

/// Stamp a header at `base` through `ObjectHeader::new` rather than by writing
/// its fields at literal offsets.
///
/// These fixtures used to poke `kind` at offset 4, `element_type` at offset 5
/// and the array length at offset 12 — the pre-`header-16` layout. The
/// 2026-08-07 shrink (24 -> 16) moved every one of them: `shape` now occupies
/// offsets 4..8 and IS the array length for an array, while `kind` and
/// `element_type` are no longer bytes at all — they are bit-fields *packed into
/// one byte* of the mark word (`KIND_TAGS_BYTE_OFFSET`, bits 0..2 and 2..6).
/// So the old writes set the array's length to `ObjectKind::Array as u8` == 1
/// and left the element type unset, and the intrinsics read a 1-element array
/// of the wrong kind.
///
/// The constructor knows the packing; a test restating it is what let this
/// drift silently in the first place.
///
/// # Safety
/// `base` must point to at least `HEADER_SIZE` writable, 8-aligned bytes.
unsafe fn write_array_header(base: *mut u8, elem: ArrayElementType, length: usize) {
    std::ptr::write(
        base as *mut ObjectHeader,
        ObjectHeader::new(
            ClassId::new(0), // primitive arrays carry ClassId(0)
            ObjectKind::Array,
            elem,
            length as u32, // Cast: fixture arrays are small
            0,             // num_slots is unused for an array shape
        ),
    );
}

/// [`write_array_header`] for an instance: `num_slots` fields, no element type.
///
/// # Safety
/// `base` must point to at least `HEADER_SIZE` writable, 8-aligned bytes.
unsafe fn write_object_header(base: *mut u8, class_id: u32, num_slots: u32) {
    std::ptr::write(
        base as *mut ObjectHeader,
        ObjectHeader::new(
            ClassId::new(class_id),
            ObjectKind::Object,
            ArrayElementType::Reference, // unused for a non-array
            0,                           // array_length is unused for an object shape
            num_slots,
        ),
    );
}

/// Build a `byte[]` heap array holding `data`.
fn make_byte_array(data: &[u8]) -> FakeObj {
    let mut obj = FakeObj::with_bytes(HEADER_SIZE + data.len());
    let base = obj.base();
    unsafe {
        write_array_header(base, ArrayElementType::Byte, data.len());
        std::ptr::copy_nonoverlapping(data.as_ptr(), base.add(HEADER_SIZE), data.len());
    }
    obj
}

/// Build a compact `java/lang/String` instance object with the given backing
/// `value` byte-array pointer, `coder` and cached `hash`. Returns the String
/// object; the caller must keep the backing `FakeObj` array alive.
fn make_string(value_ptr: i64, coder: i32, hash: i32) -> FakeObj {
    make_string_with_cid(value_ptr, coder, hash, STRING_CLASS_ID)
}

/// Like [`make_string`] but stamps an arbitrary `class_id` into the
/// `ObjectHeader` — lets the CharSequence-guard tests forge a non-String
/// receiver (a String-shaped object with the "wrong" class id).
fn make_string_with_cid(value_ptr: i64, coder: i32, hash: i32, class_id: u32) -> FakeObj {
    // 3 field cells: value (0), coder (1), hash (2).
    let mut obj = FakeObj::with_bytes(HEADER_SIZE + 3 * SLOT_SIZE);
    let base = obj.base();
    unsafe {
        write_object_header(base, class_id, 3);
        // A `Value` field cell: tag (u32) at offset 0. An 8-byte payload
        // (Object pointer) lives at FIELD_CELL_PAYLOAD64_OFFSET (8); a 4-byte
        // payload (Int) at FIELD_CELL_PAYLOAD32_OFFSET (4).
        let write_ref_cell = |idx: usize, payload: i64| {
            let cell = base.add(HEADER_SIZE + idx * SLOT_SIZE);
            std::ptr::copy_nonoverlapping(4u32.to_le_bytes().as_ptr(), cell, 4); // tag=Object
            std::ptr::copy_nonoverlapping(payload.to_le_bytes().as_ptr(), cell.add(8), 8);
        };
        let write_int_cell = |idx: usize, payload: i32| {
            let cell = base.add(HEADER_SIZE + idx * SLOT_SIZE);
            std::ptr::copy_nonoverlapping(0u32.to_le_bytes().as_ptr(), cell, 4); // tag=Int
            std::ptr::copy_nonoverlapping(payload.to_le_bytes().as_ptr(), cell.add(4), 4);
        };
        write_ref_cell(0, value_ptr); // value : Object → byte[] ptr
        write_int_cell(1, coder); // coder : Int
        write_int_cell(2, hash); // hash  : Int
    }
    obj
}

/// Encode a Rust `&str` into a compact-String `byte[]`: LATIN1 (1 byte/char)
/// when every code point fits in a byte, else UTF-16 little-endian. Returns
/// `(bytes, coder)`.
fn encode(s: &str) -> (Vec<u8>, i32) {
    let units: Vec<u16> = s.encode_utf16().collect();
    if units.iter().all(|&u| u <= 0xFF) {
        (units.iter().map(|&u| u as u8).collect(), 0)
    } else {
        let mut bytes = Vec::with_capacity(units.len() * 2);
        for u in units {
            bytes.push((u & 0xFF) as u8);
            bytes.push((u >> 8) as u8);
        }
        (bytes, 1)
    }
}

/// Host reference for `String.length()`.
fn ref_length(s: &str) -> i32 {
    s.encode_utf16().count() as i32
}

/// Host reference for `String.charAt(i)`.
fn ref_char_at(s: &str, i: usize) -> i32 {
    s.encode_utf16().nth(i).unwrap() as i32
}

/// Host reference for `String.hashCode()` — the JDK `h = 31*h + c` polynomial
/// over the UTF-16 code units, with `i32` wrapping arithmetic.
fn ref_hash(s: &str) -> i32 {
    let mut h: i32 = 0;
    for c in s.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(c as i32);
    }
    h
}

/// JIT-compile a single-arg-receiver wrapper `int f(String this)` whose body
/// is `aload_0; invokevirtual <method>; xreturn`. `ret` is the xreturn opcode
/// (`0xac` ireturn for length/hashCode/isEmpty/charAt — all int-category).
fn compile_unary(name: &str, descriptor: &str) -> Option<impl Fn(i64) -> i64> {
    let entry =
        try_resolve_string_intrinsic("java/lang/String", name, descriptor, Some(string_layout()))?
            .0;
    // aload_0 (2a), invokevirtual (b6 00 01), ireturn (ac)
    let code: Vec<u8> = vec![0x2a, 0xb6, 0x00, 0x01, 0xac, 0, 0];
    let compiled = compile(
        &code,
        code.len(),
        1, // num_params: the receiver
        1, // max_locals
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![(
            1,
            JitDirectCall {
                entry,
                needs_context: false,
                num_params: 0, // receiver excluded
                return_type: b'I',
                guard_class_id: 0,
            },
        )],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &helpers(),
        HashSet::new(),
        HashMap::new(),
        Some(string_layout()),
    )?;
    Some(move |this: i64| unsafe { compiled.try_call(&[this]).expect("test JIT call") })
}

/// JIT-compile `int f(String this, int idx)` whose body is
/// `aload_0; iload_1; invokevirtual charAt; ireturn`.
fn compile_char_at() -> impl Fn(i64, i32) -> i64 {
    let entry =
        try_resolve_string_intrinsic("java/lang/String", "charAt", "(I)C", Some(string_layout()))
            .expect("charAt must register with a layout")
            .0;
    // aload_0 (2a), iload_1 (1b), invokevirtual (b6 00 01), ireturn (ac)
    let code: Vec<u8> = vec![0x2a, 0x1b, 0xb6, 0x00, 0x01, 0xac, 0, 0];
    let compiled = compile(
        &code,
        code.len(),
        2, // receiver + index
        2,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![(
            2,
            JitDirectCall {
                entry,
                needs_context: false,
                num_params: 1, // index (receiver excluded)
                return_type: b'C',
                guard_class_id: 0,
            },
        )],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &helpers(),
        HashSet::new(),
        HashMap::new(),
        Some(string_layout()),
    )
    .expect("charAt wrapper compilation failed");
    move |this: i64, idx: i32| unsafe {
        compiled
            .try_call(&[this, idx as i64])
            .expect("test JIT call")
    }
}

// --- matcher integration --------------------------------------------------

#[test]
fn string_access_registered_only_with_a_layout() {
    let l = Some(string_layout());
    for &(name, desc) in &[
        ("length", "()I"),
        ("isEmpty", "()Z"),
        ("charAt", "(I)C"),
        ("hashCode", "()I"),
    ] {
        // With a layout → registered.
        assert!(
            try_resolve_string_intrinsic("java/lang/String", name, desc, l).is_some(),
            "{name}{desc} must register when a StringFieldLayout is present",
        );
        // Without a layout → bail to native dispatch.
        assert!(
            try_resolve_string_intrinsic("java/lang/String", name, desc, None).is_none(),
            "{name}{desc} must NOT register without a layout",
        );
        // The layout-free matcher never registers a String method.
        assert!(
            cratonvm_jit::try_resolve_intrinsic("java/lang/String", name, desc).is_none(),
            "the 3-arg try_resolve_intrinsic must never register a String method",
        );
    }
}

#[test]
fn string_access_bails_without_a_coder_field() {
    // The legacy `char[]` String layout has no `coder` field; every String
    // intrinsic decodes via `coder`, so such a layout must bail.
    let no_coder = StringFieldLayout::new(0, None, 1, STRING_CLASS_ID);
    assert!(!no_coder.has_coder);
    assert!(
        try_resolve_string_intrinsic("java/lang/String", "length", "()I", Some(no_coder)).is_none(),
        "length must bail when the layout has no coder field",
    );
}

#[test]
fn string_access_ignores_non_string_classes() {
    assert!(
        try_resolve_string_intrinsic(
            "java/lang/StringBuilder",
            "length",
            "()I",
            Some(string_layout()),
        )
        .is_none(),
        "only java/lang/String is intrinsified by this family",
    );
}

// --- length ---------------------------------------------------------------

#[test]
fn string_length_latin1_and_utf16() {
    let f = compile_unary("length", "()I").expect("length must register");
    for s in ["", "a", "hello", "0123456789abcdef", "caf\u{e9}"] {
        let (bytes, coder) = encode(s);
        let arr = make_byte_array(&bytes);
        let strobj = make_string(arr.ptr(), coder, 0);
        let got = f(strobj.ptr()) as i32;
        assert_eq!(got, ref_length(s), "length({s:?}) coder={coder}");
    }
    // A genuine UTF-16 string (code point > 0xFF forces 2-byte coder).
    let s = "A\u{4e2d}Z"; // 'A', CJK char, 'Z'
    let (bytes, coder) = encode(s);
    assert_eq!(coder, 1, "this string must be UTF-16 coded");
    let arr = make_byte_array(&bytes);
    let strobj = make_string(arr.ptr(), coder, 0);
    assert_eq!(f(strobj.ptr()) as i32, 3);
}

// --- isEmpty --------------------------------------------------------------

#[test]
fn string_is_empty() {
    let f = compile_unary("isEmpty", "()Z").expect("isEmpty must register");
    for (s, expect) in [("", 1i64), ("x", 0), ("hello", 0)] {
        let (bytes, coder) = encode(s);
        let arr = make_byte_array(&bytes);
        let strobj = make_string(arr.ptr(), coder, 0);
        assert_eq!(f(strobj.ptr()), expect, "isEmpty({s:?})");
    }
}

// --- charAt ---------------------------------------------------------------

#[test]
fn string_char_at_latin1() {
    let f = compile_char_at();
    let s = "hello world";
    let (bytes, coder) = encode(s);
    assert_eq!(coder, 0);
    let arr = make_byte_array(&bytes);
    let strobj = make_string(arr.ptr(), coder, 0);
    for i in 0..s.len() {
        assert_eq!(
            f(strobj.ptr(), i as i32) as i32,
            ref_char_at(s, i),
            "charAt {i}"
        );
    }
}

#[test]
fn string_char_at_utf16() {
    let f = compile_char_at();
    let s = "A\u{4e2d}\u{00e9}Z"; // mixed, forces UTF-16
    let (bytes, coder) = encode(s);
    assert_eq!(coder, 1);
    let arr = make_byte_array(&bytes);
    let strobj = make_string(arr.ptr(), coder, 0);
    let count = s.encode_utf16().count();
    for i in 0..count {
        assert_eq!(
            f(strobj.ptr(), i as i32) as i32,
            ref_char_at(s, i),
            "charAt {i}"
        );
    }
}

#[test]
fn string_char_at_out_of_bounds_deopts() {
    let _guard = deopt_lock();
    let f = compile_char_at();
    let (bytes, coder) = encode("abc");
    let arr = make_byte_array(&bytes);
    let strobj = make_string(arr.ptr(), coder, 0);
    // index == length and a negative index must both trap, not read OOB.
    for bad in [3i32, -1, 999] {
        let before = clear_deopt_signals();
        let r = f(strobj.ptr(), bad);
        assert_eq!(
            r,
            i64::MIN,
            "OOB charAt({bad}) must return the deopt sentinel"
        );
        assert_one_deopt_after(before, &format!("OOB charAt({bad})"));
    }
}

// --- hashCode -------------------------------------------------------------

#[test]
fn string_hash_code_computes_when_cache_zero() {
    let f = compile_unary("hashCode", "()I").expect("hashCode must register");
    for s in [
        "",
        "a",
        "hello",
        "The quick brown fox",
        "caf\u{e9}",
        "A\u{4e2d}Z",
    ] {
        let (bytes, coder) = encode(s);
        let arr = make_byte_array(&bytes);
        // hash cache = 0 → recompute.
        let strobj = make_string(arr.ptr(), coder, 0);
        assert_eq!(f(strobj.ptr()) as i32, ref_hash(s), "hashCode({s:?})");
    }
}

#[test]
fn string_hash_code_returns_cached_value() {
    let f = compile_unary("hashCode", "()I").expect("hashCode must register");
    // Non-zero cached hash must be returned verbatim (lazy-cache semantics):
    // a deliberately "wrong" cached value proves the cache short-circuit is
    // taken rather than the recompute path.
    let (bytes, coder) = encode("hello");
    let arr = make_byte_array(&bytes);
    let bogus_cached = 0x1234_5678;
    let strobj = make_string(arr.ptr(), coder, bogus_cached);
    assert_eq!(
        f(strobj.ptr()) as i32,
        bogus_cached,
        "a non-zero cached hash must be returned without recomputing",
    );
}

// --- null receiver --------------------------------------------------------

#[test]
fn string_length_null_receiver_deopts() {
    let _guard = deopt_lock();
    let f = compile_unary("length", "()I").expect("length must register");
    let before = clear_deopt_signals();
    let r = f(0); // null receiver
    assert_eq!(
        r,
        i64::MIN,
        "null-receiver length must return the deopt sentinel"
    );
    assert_one_deopt_after(before, "null-receiver length");
}

// --- CharSequence-typed call sites (receiver class-id guard) ---------------

/// A class id distinct from `STRING_CLASS_ID`, used to forge a non-String
/// CharSequence receiver (e.g. a StringBuilder).
const NON_STRING_CLASS_ID: u32 = 0x0099_0099;

/// Compile `char f(CharSequence this, int idx)` for a `java/lang/CharSequence`
/// `charAt` call site. The site carries the String class-id guard, so the
/// inline String-layout decode runs only for a real String receiver and
/// deopts otherwise.
fn compile_charseq_char_at() -> impl Fn(i64, i32) -> i64 {
    let (entry, _np, _ret, guard) = try_resolve_string_intrinsic(
        "java/lang/CharSequence",
        "charAt",
        "(I)C",
        Some(string_layout()),
    )
    .expect("CharSequence.charAt must register with a layout");
    assert_eq!(
        guard, STRING_CLASS_ID,
        "a CharSequence site must carry the String class-id guard",
    );
    // aload_0 (2a), iload_1 (1b), invokevirtual (b6 00 01), ireturn (ac)
    let code: Vec<u8> = vec![0x2a, 0x1b, 0xb6, 0x00, 0x01, 0xac, 0, 0];
    let compiled = compile(
        &code,
        code.len(),
        2,
        2,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![(
            2,
            JitDirectCall {
                entry,
                needs_context: false,
                num_params: 1,
                return_type: b'C',
                guard_class_id: guard,
            },
        )],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &helpers(),
        HashSet::new(),
        HashMap::new(),
        Some(string_layout()),
    )
    .expect("CharSequence.charAt wrapper compilation failed");
    move |this: i64, idx: i32| unsafe {
        compiled
            .try_call(&[this, idx as i64])
            .expect("test JIT call")
    }
}

#[test]
fn charseq_access_registers_only_accessors_with_string_guard() {
    let l = Some(string_layout());
    // charAt/length/isEmpty are declared on CharSequence → intrinsified,
    // carrying the String class-id guard.
    for &(name, desc) in &[("length", "()I"), ("isEmpty", "()Z"), ("charAt", "(I)C")] {
        let r = try_resolve_string_intrinsic("java/lang/CharSequence", name, desc, l);
        let (_entry, _np, _ret, guard) =
            r.unwrap_or_else(|| panic!("CharSequence.{name}{desc} must register with a layout"));
        assert_eq!(
            guard, STRING_CLASS_ID,
            "CharSequence.{name}{desc} must carry the String class-id guard",
        );
    }
    // String-specific methods are NOT on CharSequence → never intrinsified
    // for a CharSequence receiver.
    for &(name, desc) in &[
        ("hashCode", "()I"),
        ("equals", "(Ljava/lang/Object;)Z"),
        ("compareTo", "(Ljava/lang/String;)I"),
        ("indexOf", "(I)I"),
        ("indexOf", "(Ljava/lang/String;)I"),
    ] {
        assert!(
            try_resolve_string_intrinsic("java/lang/CharSequence", name, desc, l).is_none(),
            "CharSequence.{name}{desc} must NOT be intrinsified (String-only)",
        );
    }
}

#[test]
fn charseq_access_bails_without_a_string_class_id() {
    // A layout whose string_class_id is 0 (String class id unknown) cannot
    // guard a CharSequence site, so it must bail to native dispatch — while a
    // String receiver (needs no guard) still resolves.
    let no_guard = StringFieldLayout::new(0, Some(1), 2, 0);
    assert!(
        try_resolve_string_intrinsic("java/lang/CharSequence", "charAt", "(I)C", Some(no_guard))
            .is_none(),
        "CharSequence.charAt must bail when there is no String class id to guard with",
    );
    assert!(
        try_resolve_string_intrinsic("java/lang/String", "charAt", "(I)C", Some(no_guard))
            .is_some(),
        "String.charAt needs no guard, so it resolves even with string_class_id == 0",
    );
}

#[test]
fn charseq_char_at_string_receiver_decodes() {
    let f = compile_charseq_char_at();
    for s in ["abc", "hello", "caf\u{e9}", "A\u{4e2d}Z"] {
        let (bytes, coder) = encode(s);
        let arr = make_byte_array(&bytes);
        // Receiver IS a real String (header class id == STRING_CLASS_ID) →
        // the guard passes and the inline decode runs.
        let strobj = make_string(arr.ptr(), coder, 0);
        for i in 0..ref_length(s) as usize {
            assert_eq!(
                f(strobj.ptr(), i as i32) as i32,
                ref_char_at(s, i),
                "CharSequence.charAt({s:?}, {i}) with a String receiver",
            );
        }
    }
}

#[test]
fn charseq_char_at_non_string_receiver_deopts() {
    let _guard = deopt_lock();
    let f = compile_charseq_char_at();
    let (bytes, coder) = encode("hello");
    let arr = make_byte_array(&bytes);
    // A String-shaped object with the WRONG class id stands in for a non-String
    // CharSequence (e.g. StringBuilder). The receiver-class-id guard must fail
    // and deopt rather than decode foreign field memory.
    let foreign = make_string_with_cid(arr.ptr(), coder, 0, NON_STRING_CLASS_ID);
    let before = clear_deopt_signals();
    let r = f(foreign.ptr(), 0);
    assert_eq!(
        r,
        i64::MIN,
        "a non-String CharSequence receiver must return the deopt sentinel",
    );
    assert_one_deopt_after(before, "CharSequence class-id guard");
}

#[test]
fn charseq_char_at_null_receiver_deopts() {
    let _guard = deopt_lock();
    let f = compile_charseq_char_at();
    let before = clear_deopt_signals();
    let r = f(0, 0); // null receiver — null check precedes the class-id guard
    assert_eq!(r, i64::MIN, "null CharSequence receiver must deopt");
    assert_one_deopt_after(before, "null-receiver CharSequence.charAt");
}

// --- COMPACT-laid-out receivers (BUG-STRING-CODER-COMPACT-20260726) --------
//
// Every test above forges a LEGACY instance (uniform 16-byte `Value` cells,
// `GC_FLAG_COMPACT` clear) behind a fake `string_class_id` that has no
// registered `CompactLayout`, so they only ever exercise the emitters' legacy
// arm. The compact arm is what production actually runs, and it was reading
// `coder` and `hash` four bytes past their real addresses: `coder` landed on
// `hash` and `hash` landed on `hashIsZero`.
//
// `coder` is 0 (LATIN1) for nearly every string and `hash` is 0 until someone
// asks for it, so the misread was invisible until a receiver's lazy hash cache
// was populated — at which point `length()` computed
// `value.length >> (hash & 31)`. H2's interned `"PUBLIC"` schema name (a
// `HashMap` key, so hashed) has `hashCode() == -1924094359`, low five bits 9:
// `6 >> 9 == 0`. H2 persisted `CREATE SEQUENCE ""."SEQ1"` and could not reopen
// the database — see
// `h2-jitban-schema-not-found-on-reconnect-FIXED.md`.

/// Dense-registry-safe class id for the compact tests. `register_class_layout`
/// indexes a dense `Vec` by class id, so this must stay small — unlike
/// [`STRING_CLASS_ID`], which is never registered.
const COMPACT_STRING_CLASS_ID: u32 = 7;

/// Register the real JDK25 `java/lang/String` compact layout for
/// [`COMPACT_STRING_CLASS_ID`]: `value:[B` at 0, `coder:B` at 8, `hash:I` at
/// 12, `hashIsZero:Z` at 16, 24-byte body — each field at its natural Java
/// width with no tag prefix. Idempotent; safe under the test harness's
/// parallel threads.
fn compact_string_class_id() -> u32 {
    use cratonvm_types::{register_class_layout, CompactLayout, FieldStorageKind};
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        register_class_layout(
            cratonvm_types::FIRST_LAYOUT_DOMAIN,
            COMPACT_STRING_CLASS_ID,
            std::sync::Arc::new(CompactLayout {
                field_offsets: vec![0, 8, 12, 16],
                is_ref: vec![true, false, false, false],
                field_kinds: vec![
                    FieldStorageKind::Reference,
                    FieldStorageKind::Byte,
                    FieldStorageKind::Int,
                    FieldStorageKind::Boolean,
                ],
                ref_offsets: vec![0],
                body_size: 24,
            }),
        );
    });
    COMPACT_STRING_CLASS_ID
}

fn compact_string_layout() -> StringFieldLayout {
    StringFieldLayout::new(0, Some(1), 2, compact_string_class_id())
}

/// Build a COMPACT `java/lang/String` instance: `GC_FLAG_COMPACT` set,
/// `num_slots == 4`, body packed exactly as `compact_string_class_id`'s
/// layout describes. `hash_is_zero` is the JDK's `hashIsZero` flag — the field
/// the buggy `hashCode()` read as if it were the cached hash.
fn make_compact_string(value_ptr: i64, coder: u8, hash: i32, hash_is_zero: bool) -> FakeObj {
    let mut obj = FakeObj::with_bytes(HEADER_SIZE + 24);
    let base = obj.base();
    unsafe {
        std::ptr::copy_nonoverlapping(COMPACT_STRING_CLASS_ID.to_le_bytes().as_ptr(), base, 4);
        *base.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET) = ObjectKind::Object as u8;
        *base.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) = cratonvm_types::GC_FLAG_COMPACT;
        std::ptr::copy_nonoverlapping(
            4u32.to_le_bytes().as_ptr(),
            base.add(cratonvm_types::NUM_SLOTS_OFFSET),
            4,
        );
        let body = base.add(HEADER_SIZE);
        std::ptr::copy_nonoverlapping(value_ptr.to_le_bytes().as_ptr(), body, 8);
        *body.add(8) = coder;
        std::ptr::copy_nonoverlapping(hash.to_le_bytes().as_ptr(), body.add(12), 4);
        *body.add(16) = hash_is_zero as u8;
    }
    obj
}

/// `compile_unary`, but against [`compact_string_layout`] and with the
/// receiver-class-id guard pointed at [`COMPACT_STRING_CLASS_ID`].
fn compile_unary_compact(name: &str, descriptor: &str) -> Option<impl Fn(i64) -> i64> {
    let layout = compact_string_layout();
    let entry = try_resolve_string_intrinsic("java/lang/String", name, descriptor, Some(layout))?.0;
    let code: Vec<u8> = vec![0x2a, 0xb6, 0x00, 0x01, 0xac, 0, 0];
    let compiled = compile(
        &code,
        code.len(),
        1,
        1,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![(
            1,
            JitDirectCall {
                entry,
                needs_context: false,
                num_params: 0,
                return_type: b'I',
                guard_class_id: 0,
            },
        )],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &helpers(),
        HashSet::new(),
        HashMap::new(),
        Some(layout),
    )?;
    Some(move |this: i64| unsafe { compiled.try_call(&[this]).expect("test JIT call") })
}

#[test]
fn compact_string_layout_offsets_are_exact_payload_addresses() {
    let l = compact_string_layout();
    // COMPACT: `HEADER_SIZE + body_offset`, nothing added.
    assert_eq!(l.value_compact_offset, HEADER_SIZE as i32, "value compact");
    assert_eq!(
        l.coder_compact_offset,
        (HEADER_SIZE + 8) as i32,
        "coder compact"
    );
    assert_eq!(
        l.hash_compact_offset,
        (HEADER_SIZE + 12) as i32,
        "hash compact"
    );
    assert!(l.coder_compact_is_byte, "coder is a natural-width byte");
    // LEGACY: uniform 16-byte cells, payload inside each.
    assert_eq!(
        l.value_legacy_offset,
        (HEADER_SIZE + 8) as i32,
        "value legacy"
    );
    assert_eq!(
        l.coder_legacy_offset,
        (HEADER_SIZE + SLOT_SIZE + 4) as i32,
        "coder legacy"
    );
    assert_eq!(
        l.hash_legacy_offset,
        (HEADER_SIZE + 2 * SLOT_SIZE + 4) as i32,
        "hash legacy"
    );
    // The old scheme derived legacy from compact by a fixed +8, which is only
    // right for field indices 0 and 1 — `hash` (index 2) came out 12 bytes low.
    assert_ne!(l.hash_legacy_offset, l.hash_compact_offset + 8);
}

#[test]
fn compact_string_length_ignores_cached_hash() {
    let f = compile_unary_compact("length", "()I").expect("length must register");
    // -1924094359 is "PUBLIC".hashCode(); its low five bits are 9, so a
    // `value.length >> hash` misread answers 0 for any string shorter than 512.
    for (s, hash) in [
        ("PUBLIC", -1_924_094_359i32),
        ("hello", 99_162_322),
        ("x", 120),
        ("", 0),
    ] {
        let (bytes, coder) = encode(s);
        let arr = make_byte_array(&bytes);
        let strobj = make_compact_string(arr.ptr(), coder as u8, hash, hash == 0);
        assert_eq!(f(strobj.ptr()) as i32, ref_length(s), "length({s:?})");
    }
    // UTF-16 receiver: `coder == 1` must be read from `coder`, not from `hash`.
    let s = "A\u{4e2d}Z";
    let (bytes, coder) = encode(s);
    assert_eq!(coder, 1);
    let arr = make_byte_array(&bytes);
    let strobj = make_compact_string(arr.ptr(), coder as u8, -1_924_094_359, false);
    assert_eq!(
        f(strobj.ptr()) as i32,
        3,
        "UTF-16 length under a cached hash"
    );
}

#[test]
fn compact_string_is_empty_ignores_cached_hash() {
    let f = compile_unary_compact("isEmpty", "()Z").expect("isEmpty must register");
    let (bytes, coder) = encode("PUBLIC");
    let arr = make_byte_array(&bytes);
    let strobj = make_compact_string(arr.ptr(), coder as u8, -1_924_094_359, false);
    assert_eq!(f(strobj.ptr()), 0, "a 6-char string is not empty");
}

#[test]
fn compact_string_hash_code_reads_hash_not_hash_is_zero() {
    let f = compile_unary_compact("hashCode", "()I").expect("hashCode must register");
    // Populated cache → returned verbatim.
    let (bytes, coder) = encode("PUBLIC");
    let arr = make_byte_array(&bytes);
    let strobj = make_compact_string(arr.ptr(), coder as u8, ref_hash("PUBLIC"), false);
    assert_eq!(f(strobj.ptr()) as i32, ref_hash("PUBLIC"), "cached hash");
    // Empty cache with `hashIsZero == true` — the buggy read returned that
    // flag (1) instead of recomputing 0.
    let empty = make_byte_array(&[]);
    let strobj = make_compact_string(empty.ptr(), 0, 0, true);
    assert_eq!(f(strobj.ptr()) as i32, 0, "\"\".hashCode()");
    // Cold cache on a non-empty string → recompute.
    let (bytes, coder) = encode("hello");
    let arr = make_byte_array(&bytes);
    let strobj = make_compact_string(arr.ptr(), coder as u8, 0, false);
    assert_eq!(f(strobj.ptr()) as i32, ref_hash("hello"), "recomputed hash");
}

#[test]
fn compact_string_char_at_decodes_through_its_own_coder() {
    let layout = compact_string_layout();
    let entry = try_resolve_string_intrinsic("java/lang/String", "charAt", "(I)C", Some(layout))
        .expect("charAt must register")
        .0;
    // aload_0, iload_1, invokevirtual, ireturn
    let code: Vec<u8> = vec![0x2a, 0x1b, 0xb6, 0x00, 0x02, 0xac, 0, 0];
    let compiled = compile(
        &code,
        code.len(),
        2,
        2,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![(
            2,
            JitDirectCall {
                entry,
                needs_context: false,
                num_params: 1,
                return_type: b'I',
                guard_class_id: 0,
            },
        )],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &helpers(),
        HashSet::new(),
        HashMap::new(),
        Some(layout),
    )
    .expect("charAt wrapper must compile");
    let f = |this: i64, i: i64| unsafe { compiled.try_call(&[this, i]).expect("test JIT call") };
    for s in ["PUBLIC", "A\u{4e2d}Z"] {
        let (bytes, coder) = encode(s);
        let arr = make_byte_array(&bytes);
        // Non-zero cached hash: the coder read must not pick it up.
        let strobj = make_compact_string(arr.ptr(), coder as u8, ref_hash(s), false);
        for i in 0..s.encode_utf16().count() {
            assert_eq!(
                f(strobj.ptr(), i as i64) as i32,
                ref_char_at(s, i),
                "charAt({s:?},{i})"
            );
        }
    }
}
