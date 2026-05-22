//! Differential tests for the STRING_SEARCH JIT intrinsic family — the
//! `java/lang/String` instance-method call-site intrinsic (Phase 3b).
//!
//! INLINED: `equals(Ljava/lang/Object;)Z`. The foundation waves (commits
//! 06bfac0 / 544cbea) added inline `getfield` + array-access codegen and the
//! `StringFieldLayout` API, making a coder-and-length-guarded raw byte
//! compare possible:
//!   * `other == null`                 → false
//!   * `this == other` (same pointer)  → true
//!   * `other`'s ObjectHeader class id != `this`'s → uncommon-trap deopt
//!     (a non-String argument; native `equals` returns false)
//!   * either backing `value` array null → deopt
//!   * `this.coder != other.coder`        → deopt (rare; native compares the
//!     decoded char slices)
//!   * `value`-array lengths differ       → false
//!   * else `REP CMPSB` over the bytes    → true iff identical.
//! Same coder + identical backing bytes ⇒ identical decoded strings, so the
//! raw byte compare is exact.
//!
//! BAILED (NOT registered — fall back to native dispatch):
//! `compareTo(Ljava/lang/String;)I`, `indexOf(I)I`,
//! `indexOf(Ljava/lang/String;)I`. They need ordered decoded-char comparison
//! or substring-search loops whose UTF-16 / legacy-char[] edge cases are not
//! worth the codegen risk (roadmap §3.4: correctness over coverage).

use cratonvm_jit::x64::compile;
use cratonvm_jit::{try_resolve_string_intrinsic, JitDirectCall, StringFieldLayout};
use cratonvm_jit_api::JitRuntimeHelpers;
use cratonvm_types::{ArrayElementType, ObjectKind, HEADER_SIZE, SLOT_SIZE};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

static TRAP_COUNT: AtomicU64 = AtomicU64::new(0);
static DEOPT_LOCK: Mutex<()> = Mutex::new(());

unsafe extern "C" fn recording_uncommon_trap(_vm: i64, _reason: i64, _bci: i64) -> i64 {
    TRAP_COUNT.fetch_add(1, Ordering::SeqCst);
    0
}

fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("STRING_SEARCH intrinsic test invoked an unwired runtime helper");
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
        invoke_dispatch: s,
        invoke_virtual_mic: s,
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
    }
}

fn string_layout() -> StringFieldLayout {
    StringFieldLayout::new(0, Some(1), 2)
}

const STRING_CLASS_ID: u32 = 0x5712_3400;
const OTHER_CLASS_ID: u32 = 0x0099_0099;

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

fn make_byte_array(data: &[u8]) -> FakeObj {
    let mut obj = FakeObj::with_bytes(HEADER_SIZE + data.len().max(1));
    let base = obj.base();
    unsafe {
        *base.add(4) = ObjectKind::Array as u8;
        *base.add(5) = ArrayElementType::Byte as u8;
        let len_le = (data.len() as u32).to_le_bytes();
        std::ptr::copy_nonoverlapping(len_le.as_ptr(), base.add(12), 4);
        if !data.is_empty() {
            std::ptr::copy_nonoverlapping(data.as_ptr(), base.add(HEADER_SIZE), data.len());
        }
    }
    obj
}

/// Build a String-like object with the given class id, backing array,
/// coder and hash.
fn make_object(class_id: u32, value_ptr: i64, coder: i32, hash: i32) -> FakeObj {
    let mut obj = FakeObj::with_bytes(HEADER_SIZE + 3 * SLOT_SIZE);
    let base = obj.base();
    unsafe {
        std::ptr::copy_nonoverlapping(class_id.to_le_bytes().as_ptr(), base, 4);
        *base.add(4) = ObjectKind::Object as u8;
        // A `Value` field cell: tag (u32) at 0; an Object payload at
        // FIELD_CELL_PAYLOAD64_OFFSET (8); an Int payload at
        // FIELD_CELL_PAYLOAD32_OFFSET (4).
        let write_ref_cell = |idx: usize, payload: i64| {
            let cell = base.add(HEADER_SIZE + idx * SLOT_SIZE);
            std::ptr::copy_nonoverlapping(4u32.to_le_bytes().as_ptr(), cell, 4);
            std::ptr::copy_nonoverlapping(payload.to_le_bytes().as_ptr(), cell.add(8), 8);
        };
        let write_int_cell = |idx: usize, payload: i32| {
            let cell = base.add(HEADER_SIZE + idx * SLOT_SIZE);
            std::ptr::copy_nonoverlapping(0u32.to_le_bytes().as_ptr(), cell, 4);
            std::ptr::copy_nonoverlapping(payload.to_le_bytes().as_ptr(), cell.add(4), 4);
        };
        write_ref_cell(0, value_ptr); // value : Object
        write_int_cell(1, coder); // coder : Int
        write_int_cell(2, hash); // hash  : Int
    }
    obj
}

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

/// JIT-compile `int f(String this, Object other)` whose body is
/// `aload_0; aload_1; invokevirtual equals; ireturn`.
fn compile_equals() -> impl Fn(i64, i64) -> i64 {
    let entry = try_resolve_string_intrinsic(
        "java/lang/String",
        "equals",
        "(Ljava/lang/Object;)Z",
        Some(string_layout()),
    )
    .expect("equals must register with a layout")
    .0;
    // aload_0 (2a), aload_1 (2b), invokevirtual (b6 00 01), ireturn (ac)
    let code: Vec<u8> = vec![0x2a, 0x2b, 0xb6, 0x00, 0x01, 0xac, 0, 0];
    let compiled = compile(
        &code,
        code.len(),
        2, // receiver + other
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
                num_params: 1, // other (receiver excluded)
                return_type: b'Z',
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
    .expect("equals wrapper compilation failed");
    move |this: i64, other: i64| unsafe { compiled.call(&[this, other]) }
}

/// Build a String object for `s` plus its backing array; return both so the
/// caller keeps them alive, along with the String pointer.
fn string_of(s: &str) -> (FakeObj, FakeObj) {
    let (bytes, coder) = encode(s);
    let arr = make_byte_array(&bytes);
    let strobj = make_object(STRING_CLASS_ID, arr.ptr(), coder, 0);
    (strobj, arr)
}

// --- matcher integration --------------------------------------------------

#[test]
fn string_equals_registered_only_with_a_layout() {
    assert!(
        try_resolve_string_intrinsic(
            "java/lang/String",
            "equals",
            "(Ljava/lang/Object;)Z",
            Some(string_layout()),
        )
        .is_some(),
        "equals must register when a StringFieldLayout is present",
    );
    assert!(
        try_resolve_string_intrinsic(
            "java/lang/String",
            "equals",
            "(Ljava/lang/Object;)Z",
            None,
        )
        .is_none(),
        "equals must NOT register without a layout",
    );
    // The 3-arg layout-free matcher never registers a String method.
    assert!(
        cratonvm_jit::try_resolve_intrinsic(
            "java/lang/String",
            "equals",
            "(Ljava/lang/Object;)Z",
        )
        .is_none(),
    );
}

#[test]
fn string_search_compare_and_index_of_are_bailed() {
    // compareTo / indexOf are intentionally NOT inlined — they must bail to
    // native dispatch even when a layout is available.
    let l = Some(string_layout());
    for &(name, desc) in &[
        ("compareTo", "(Ljava/lang/String;)I"),
        ("indexOf", "(I)I"),
        ("indexOf", "(Ljava/lang/String;)I"),
    ] {
        assert!(
            try_resolve_string_intrinsic("java/lang/String", name, desc, l).is_none(),
            "{name}{desc} must remain bailed to native dispatch",
        );
    }
}

// --- equals: equal & unequal strings --------------------------------------

#[test]
fn string_equals_equal_strings() {
    let f = compile_equals();
    for s in ["", "a", "hello", "The quick brown fox", "caf\u{e9}", "A\u{4e2d}Z"] {
        let (a, _a_arr) = string_of(s);
        let (b, _b_arr) = string_of(s);
        assert_eq!(f(a.ptr(), b.ptr()), 1, "equals({s:?}, copy) must be true");
    }
}

#[test]
fn string_equals_unequal_same_length() {
    let f = compile_equals();
    // Same length, differing content → false (REP CMPSB mismatch).
    let (a, _aa) = string_of("hello");
    let (b, _bb) = string_of("world");
    assert_eq!(f(a.ptr(), b.ptr()), 0);
}

#[test]
fn string_equals_unequal_different_length() {
    let f = compile_equals();
    // Differing backing-array lengths → false via the length-mismatch path.
    let (a, _aa) = string_of("hi");
    let (b, _bb) = string_of("hello");
    assert_eq!(f(a.ptr(), b.ptr()), 0);
}

#[test]
fn string_equals_same_reference_is_true() {
    let f = compile_equals();
    let (a, _aa) = string_of("anything");
    // this == other (identical pointer) → true via the pointer-equal path.
    assert_eq!(f(a.ptr(), a.ptr()), 1);
}

#[test]
fn string_equals_null_argument_is_false() {
    let f = compile_equals();
    let (a, _aa) = string_of("hello");
    // other == null → false; no trap (handled inline).
    assert_eq!(f(a.ptr(), 0), 0);
}

#[test]
fn string_equals_utf16_strings() {
    let f = compile_equals();
    let s = "A\u{4e2d}\u{00e9}Z";
    let (a, _aa) = string_of(s);
    let (b, _bb) = string_of(s);
    assert_eq!(f(a.ptr(), b.ptr()), 1, "UTF-16 equal strings must compare equal");
    // A genuinely different UTF-16 string of the same length.
    let (c, _cc) = string_of("A\u{4e2d}\u{00e8}Z");
    assert_eq!(f(a.ptr(), c.ptr()), 0);
}

// --- equals: deopt edges --------------------------------------------------

#[test]
fn string_equals_non_string_argument_deopts() {
    let _guard = DEOPT_LOCK.lock().unwrap();
    let f = compile_equals();
    let (a, _aa) = string_of("hello");
    // `other` has a different class id (a non-String object). The class-id
    // guard must trap so the interpreter's native equals returns false.
    let (bytes, coder) = encode("hello");
    let arr = make_byte_array(&bytes);
    let other = make_object(OTHER_CLASS_ID, arr.ptr(), coder, 0);
    let before = TRAP_COUNT.load(Ordering::SeqCst);
    let r = f(a.ptr(), other.ptr());
    assert_eq!(r, i64::MIN, "non-String argument must return the deopt sentinel");
    assert_eq!(
        TRAP_COUNT.load(Ordering::SeqCst),
        before + 1,
        "non-String argument must fire exactly one uncommon trap",
    );
}

#[test]
fn string_equals_coder_mismatch_deopts() {
    let _guard = DEOPT_LOCK.lock().unwrap();
    let f = compile_equals();
    // Two Strings with different `coder` values must trap (the inline byte
    // compare is only valid for matching coders; native equals decodes).
    let (a, _aa) = string_of("hi"); // LATIN1, coder 0
    let (bytes, _coder) = encode("hi");
    let arr = make_byte_array(&bytes);
    let b = make_object(STRING_CLASS_ID, arr.ptr(), 1, 0); // coder forced to 1
    let before = TRAP_COUNT.load(Ordering::SeqCst);
    let r = f(a.ptr(), b.ptr());
    assert_eq!(r, i64::MIN, "coder mismatch must return the deopt sentinel");
    assert_eq!(
        TRAP_COUNT.load(Ordering::SeqCst),
        before + 1,
        "coder mismatch must fire exactly one uncommon trap",
    );
}
