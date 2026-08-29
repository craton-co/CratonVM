// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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
//! ALSO INLINED (Phase 3b follow-up): `compareTo(Ljava/lang/String;)I`,
//! `indexOf(I)I`, `indexOf(Ljava/lang/String;)I`. Each character is decoded
//! through its OWN `coder` byte, so every LATIN1/UTF16 combination —
//! including mixed-coder receiver/argument pairs — is handled inline with no
//! coder-mismatch deopt. The deopt stub is reached only for a null receiver,
//! a null String argument, or a null backing `value` array. The codegen is
//! bit-identical to `native-builtins/src/lang_string.rs`:
//!   * compareTo — unsigned-char difference at the first mismatch, else
//!     len1 - len2;
//!   * indexOf(I) — scan for `(ch & 0xFFFF)` from index 0 (matches native,
//!     which masks the argument to one code unit — no surrogate handling);
//!   * indexOf(String) — naive O(n*m) search from 0; empty needle → 0.

use cratonvm_jit::x64::compile;
use cratonvm_jit::{try_resolve_string_intrinsic, JitDirectCall, StringFieldLayout};
use cratonvm_jit_api::JitRuntimeHelpers;
use cratonvm_types::{ArrayElementType, ClassId, ObjectHeader, ObjectKind, HEADER_SIZE, SLOT_SIZE};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

static TRAP_COUNT: AtomicU64 = AtomicU64::new(0);
static DEOPT_LOCK: Mutex<()> = Mutex::new(());

unsafe extern "C" fn recording_uncommon_trap(_vm: i64, _reason: i64, _bci: i64) -> i64 {
    TRAP_COUNT.fetch_add(1, Ordering::SeqCst);
    0
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
        panic!("STRING_SEARCH intrinsic test invoked an unwired runtime helper");
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

const STRING_CLASS_ID: u32 = 0x5712_3400;
const OTHER_CLASS_ID: u32 = 0x0099_0099;

fn string_layout() -> StringFieldLayout {
    StringFieldLayout::new(0, Some(1), 2, STRING_CLASS_ID)
}

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
        // Through the constructor, not at literal offsets — see the note in
        // `intrinsic_string_access.rs::write_array_header`. The pre-`header-16`
        // offsets 4/5/12 set the length to `ObjectKind::Array as u8` == 1 and
        // left the element type unset.
        std::ptr::write(
            base as *mut ObjectHeader,
            ObjectHeader::new(
                ClassId::new(0),
                ObjectKind::Array,
                ArrayElementType::Byte,
                data.len() as u32, // Cast: fixture arrays are small
                0,
            ),
        );
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
        std::ptr::write(
            base as *mut ObjectHeader,
            ObjectHeader::new(
                ClassId::new(class_id),
                ObjectKind::Object,
                ArrayElementType::Reference, // unused for a non-array
                0,
                3, // value, coder, hash
            ),
        );
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
    move |this: i64, other: i64| unsafe {
        compiled.try_call(&[this, other]).expect("test JIT call")
    }
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
        try_resolve_string_intrinsic("java/lang/String", "equals", "(Ljava/lang/Object;)Z", None,)
            .is_none(),
        "equals must NOT register without a layout",
    );
    // The 3-arg layout-free matcher never registers a String method.
    assert!(cratonvm_jit::try_resolve_intrinsic(
        "java/lang/String",
        "equals",
        "(Ljava/lang/Object;)Z",
    )
    .is_none(),);
}

#[test]
fn string_search_compare_and_index_of_str_register_with_a_layout() {
    // compareTo / indexOf(String) are inlined — they register when a
    // StringFieldLayout is present and bail (return None) without one.
    //
    // `indexOf(I)` is NOT in this list, and its absence is asserted rather
    // than merely implied — see `string_index_of_char_is_not_intrinsified`.
    for &(name, desc) in &[
        ("compareTo", "(Ljava/lang/String;)I"),
        ("indexOf", "(Ljava/lang/String;)I"),
    ] {
        assert!(
            try_resolve_string_intrinsic("java/lang/String", name, desc, Some(string_layout()),)
                .is_some(),
            "{name}{desc} must register with a StringFieldLayout",
        );
        assert!(
            try_resolve_string_intrinsic("java/lang/String", name, desc, None).is_none(),
            "{name}{desc} must NOT register without a layout",
        );
        // The 3-arg layout-free matcher never registers a String method.
        assert!(cratonvm_jit::try_resolve_intrinsic("java/lang/String", name, desc).is_none(),);
    }
}

/// JIT-compile `int f(String this, String other)` whose body is
/// `aload_0; aload_1; invokevirtual <name>; ireturn` — used for the
/// `compareTo` / `indexOf(String)` object-argument intrinsics.
fn compile_obj_arg(name: &str, descriptor: &str) -> impl Fn(i64, i64) -> i64 {
    let entry =
        try_resolve_string_intrinsic("java/lang/String", name, descriptor, Some(string_layout()))
            .expect("intrinsic must register with a layout")
            .0;
    // aload_0 (2a), aload_1 (2b), invokevirtual (b6 00 01), ireturn (ac).
    let code: Vec<u8> = vec![0x2a, 0x2b, 0xb6, 0x00, 0x01, 0xac, 0, 0];
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
        Some(string_layout()),
    )
    .expect("object-arg wrapper compilation failed");
    move |this: i64, other: i64| unsafe {
        compiled.try_call(&[this, other]).expect("test JIT call")
    }
}

/// Reference `compareTo` — the `native_string_compare_to` oracle.
fn oracle_compare_to(a: &str, b: &str) -> i32 {
    let ua: Vec<u16> = a.encode_utf16().collect();
    let ub: Vec<u16> = b.encode_utf16().collect();
    let min = ua.len().min(ub.len());
    for i in 0..min {
        let d = ua[i] as i32 - ub[i] as i32;
        if d != 0 {
            return d;
        }
    }
    ua.len() as i32 - ub.len() as i32
}

/// Reference `indexOf(String)` — the `native_string_index_of_str` oracle.
fn oracle_index_of_str(h: &str, n: &str) -> i32 {
    let uh: Vec<u16> = h.encode_utf16().collect();
    let un: Vec<u16> = n.encode_utf16().collect();
    if un.is_empty() {
        return 0;
    }
    if un.len() > uh.len() {
        return -1;
    }
    for i in 0..=(uh.len() - un.len()) {
        if uh[i..i + un.len()] == un[..] {
            return i as i32;
        }
    }
    -1
}

// --- compareTo ------------------------------------------------------------

#[test]
fn string_compare_to_differential() {
    let f = compile_obj_arg("compareTo", "(Ljava/lang/String;)I");
    // (a, b) pairs spanning equal / less / greater, prefix/suffix, empty,
    // and both coders (LATIN1 ASCII pairs, UTF-16 CJK pairs, mixed-coder).
    let cases = [
        ("", ""),
        ("a", ""),
        ("", "a"),
        ("abc", "abc"),
        ("abc", "abd"),
        ("abd", "abc"),
        ("abc", "ab"),
        ("ab", "abc"),
        ("hello", "world"),
        ("caf\u{e9}", "caf\u{e9}"),   // both UTF-16-free LATIN1
        ("A\u{4e2d}Z", "A\u{4e2d}Z"), // both UTF-16
        ("A\u{4e2d}Z", "A\u{4e2e}Z"), // UTF-16, differ at index 1
        ("abc", "A\u{4e2d}c"),        // mixed: LATIN1 vs UTF-16
        ("A\u{4e2d}c", "abc"),        // mixed, reversed
        ("\u{ff}", "\u{100}"),        // LATIN1 0xFF vs UTF-16 0x100
    ];
    for (a, b) in cases {
        let (sa, _aa) = string_of(a);
        let (sb, _bb) = string_of(b);
        let got = f(sa.ptr(), sb.ptr()) as i32;
        let want = oracle_compare_to(a, b);
        // Native compares raw magnitudes; the JIT must reproduce the exact
        // sign AND value (callers rely on the difference, not just sign).
        assert_eq!(got, want, "compareTo({a:?}, {b:?})");
    }
}

#[test]
fn string_compare_to_null_argument_deopts() {
    let _guard = deopt_lock();
    let f = compile_obj_arg("compareTo", "(Ljava/lang/String;)I");
    let (a, _aa) = string_of("hello");
    let before = clear_deopt_signals();
    // null argument → deopt (the native re-run throws NullPointerException).
    assert_eq!(f(a.ptr(), 0), i64::MIN, "null argument must deopt");
    assert_one_deopt_after(before, "compareTo null argument");
}

// --- indexOf(I) -----------------------------------------------------------

/// E27-1 N2b: `indexOf(I)` registers again, but the RANGE screen that makes it
/// correct is not here — it is `prev_insn_int_const` in the backend, which sees
/// the operand this door never does. So this pins the door's half only:
/// registration is layout-gated exactly like its siblings.
#[test]
fn string_index_of_char_registers_with_a_layout() {
    assert!(
        try_resolve_string_intrinsic("java/lang/String", "indexOf", "(I)I", Some(string_layout()))
            .is_some(),
        "indexOf(I) must register with a StringFieldLayout (E27-1 N2b)",
    );
    assert!(
        try_resolve_string_intrinsic("java/lang/String", "indexOf", "(I)I", None).is_none(),
        "indexOf(I) must NOT register without a layout",
    );
    // The 3-arg layout-free matcher never registers a String method.
    assert!(cratonvm_jit::try_resolve_intrinsic("java/lang/String", "indexOf", "(I)I").is_none());
}

/// The screen itself: which call sites the backend will inline.
///
/// This is the assertion that keeps N2b honest. The inline scan is only the
/// JDK's answer for a needle in `0..=0xFFFF`; outside it the rule is
/// `Character.isValidCodePoint` plus a surrogate-PAIR match. A site whose
/// needle is not a provable BMP constant must therefore NOT be intrinsified —
/// and must not be deoptimised either, which is why the screen is a compile-
/// time filter rather than a runtime guard. See `prev_insn_int_const`.
#[test]
fn index_of_char_screen_admits_only_constant_bmp_needles() {
    use cratonvm_jit::x64::prev_insn_int_const_for_test as screen;
    // `bipush 'a'; invokevirtual` — the shape of `s.indexOf('a')`.
    let code = [0x2a, 0x10, b'a', 0xb6, 0x00, 0x01, 0xac];
    assert_eq!(screen(&code, code.len(), 3), Some('a' as i32));
    // `sipush 0xFFFF` is -1 as a SIGNED 16-bit immediate, which is exactly the
    // measured `"\u{FFFF}q".indexOf(-1)` row: it must be rejected, not masked
    // back to 0xFFFF.
    let neg = [0x2a, 0x11, 0xFF, 0xFF, 0xb6, 0x00, 0x01, 0xac];
    assert_eq!(screen(&neg, neg.len(), 4), Some(-1));
    // sipush of a large BMP value stays in range.
    let big = [0x2a, 0x11, 0x4e, 0x2d, 0xb6, 0x00, 0x01, 0xac];
    assert_eq!(screen(&big, big.len(), 4), Some(0x4e2d));
    // iconst_0 .. iconst_5 and iconst_m1.
    for (op, want) in [(0x02u8, -1i32), (0x03, 0), (0x08, 5)] {
        let c = [0x2a, op, 0xb6, 0x00, 0x01, 0xac];
        assert_eq!(screen(&c, c.len(), 2), Some(want), "opcode {op:#x}");
    }
    // A NON-constant needle — `iload_1` — is not screened in, so the site is
    // never intrinsified. This is the row that stops the cliff.
    let var = [0x2a, 0x1b, 0xb6, 0x00, 0x01, 0xac];
    assert_eq!(screen(&var, var.len(), 2), None);
    // `ldc` is deliberately not decoded (no constant pool at this layer):
    // a missed optimisation, never a wrong answer.
    let ldc = [0x2a, 0x12, 0x07, 0xb6, 0x00, 0x01, 0xac];
    assert_eq!(screen(&ldc, ldc.len(), 3), None);
    // pc 0 has no previous instruction.
    assert_eq!(screen(&var, var.len(), 0), None);
}

/// JIT-compile `int f(String this)` whose body is
/// `aload_0; sipush <ch>; invokevirtual indexOf(I)I; ireturn` — one compiled
/// wrapper per CONSTANT needle, which is the only shape E27-1 N2b's screen
/// admits. (The pre-N2b harness passed the needle in `iload_1`; that site is
/// now correctly declined, so it can no longer reach the intrinsic at all.)
fn compile_index_of_const_char(ch: u16) -> impl Fn(i64) -> i64 {
    let entry =
        try_resolve_string_intrinsic("java/lang/String", "indexOf", "(I)I", Some(string_layout()))
            .expect("indexOf(I) must register with a layout")
            .0;
    // aload_0 (2a), sipush ch (11 hi lo), invokevirtual (b6 00 01), ireturn (ac).
    let code: Vec<u8> = vec![
        0x2a,
        0x11,
        (ch >> 8) as u8,
        (ch & 0xFF) as u8,
        0xb6,
        0x00,
        0x01,
        0xac,
        0,
        0,
    ];
    let compiled = compile(
        &code,
        code.len(),
        2,
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
            4,
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
        Some(string_layout()),
    )
    .expect("indexOf(I) wrapper compilation failed");
    move |this: i64| unsafe { compiled.try_call(&[this]).expect("test JIT call") }
}

/// The emitted scan, against the JDK's rule — not against the sibling
/// implementation.
///
/// The oracle here is a UTF-16 code-unit position, which for a needle in
/// `0..=0xFFFF` IS `code_point_needle`'s answer (single code unit, lone
/// surrogates included). That equivalence is precisely what the screen buys,
/// and it is why this oracle is legitimate where the pre-N2b one — which
/// derived its expected value from `(ch & 0xFFFF)` and so asserted that
/// `"a…".indexOf(0x10061)` finds `'a'` — was not.
#[test]
fn string_index_of_const_char_differential() {
    let haystacks = [
        "",
        "a",
        "hello",
        "banana",
        "caf\u{e9}",
        "A\u{4e2d}Z\u{4e2d}",
        // A supplementary char in the HAYSTACK is fine and is worth pinning:
        // the needle is BMP, so the answer is a UTF-16 index that must count
        // the surrogate pair as TWO units. This is the row the old
        // `chars().enumerate()` native family got wrong.
        "x\u{10437}yz",
    ];
    let needles: [u16; 6] = [
        'a' as u16, 'z' as u16, 'o' as u16, 0x00e9, 0x4e2d,
        0xFFFF, // a valid non-character, and a BMP one: scanned, not rejected
    ];
    for &ch in &needles {
        let f = compile_index_of_const_char(ch);
        for h in haystacks {
            let (s, _ss) = string_of(h);
            let got = f(s.ptr()) as i32;
            let want = h
                .encode_utf16()
                .position(|c| c == ch)
                .map(|i| i as i32)
                .unwrap_or(-1);
            assert_eq!(got, want, "indexOf({h:?}, {ch:#x})");
        }
    }
}

/// The pinned row: a BMP needle must be found at its UTF-16 index, counting a
/// preceding supplementary character as the TWO code units it is.
#[test]
fn string_index_of_const_char_counts_utf16_units_not_code_points() {
    let f = compile_index_of_const_char('z' as u16);
    let (s, _ss) = string_of("x\u{10437}yz");
    // x=0, surrogate pair=1,2, y=3, z=4. A code-point index would say 3.
    assert_eq!(
        f(s.ptr()) as i32,
        4,
        "must be a UTF-16 index, not a code-point one"
    );
}

#[test]
fn string_index_of_const_char_null_receiver_deopts() {
    let _guard = deopt_lock();
    let f = compile_index_of_const_char('a' as u16);
    let before = clear_deopt_signals();
    assert_eq!(f(0), i64::MIN, "null receiver must deopt");
    assert_one_deopt_after(before, "indexOf(I) null receiver");
}

// --- indexOf(String) ------------------------------------------------------

#[test]
fn string_index_of_str_differential() {
    let f = compile_obj_arg("indexOf", "(Ljava/lang/String;)I");
    let cases = [
        ("", ""),                             // empty needle → 0
        ("hello", ""),                        // empty needle → 0
        ("", "x"),                            // needle longer than haystack → -1
        ("hello", "hello"),                   // whole-string match
        ("hello", "he"),                      // prefix
        ("hello", "lo"),                      // suffix
        ("hello", "ell"),                     // interior
        ("hello", "xyz"),                     // no match
        ("hello", "hellox"),                  // needle longer → -1
        ("banana", "ana"),                    // multi-occurrence → first index
        ("aaaa", "aa"),                       // overlapping occurrences → 0
        ("abcabc", "bc"),                     // repeated
        ("A\u{4e2d}B\u{4e2d}C", "\u{4e2d}B"), // UTF-16 haystack + needle
        ("caf\u{e9} bar", "\u{e9} b"),        // LATIN1 both
        ("abcdef", "A\u{4e2d}"),              // mixed coder, no match
        ("A\u{4e2d}cdef", "cd"),              // UTF-16 haystack, LATIN1 needle
    ];
    for (h, n) in cases {
        let (sh, _hh) = string_of(h);
        let (sn, _nn) = string_of(n);
        let got = f(sh.ptr(), sn.ptr()) as i32;
        let want = oracle_index_of_str(h, n);
        assert_eq!(got, want, "indexOf({h:?}, {n:?})");
    }
}

// --- equals: equal & unequal strings --------------------------------------

#[test]
fn string_equals_equal_strings() {
    let f = compile_equals();
    for s in [
        "",
        "a",
        "hello",
        "The quick brown fox",
        "caf\u{e9}",
        "A\u{4e2d}Z",
    ] {
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
    assert_eq!(
        f(a.ptr(), b.ptr()),
        1,
        "UTF-16 equal strings must compare equal"
    );
    // A genuinely different UTF-16 string of the same length.
    let (c, _cc) = string_of("A\u{4e2d}\u{00e8}Z");
    assert_eq!(f(a.ptr(), c.ptr()), 0);
}

// --- equals: deopt edges --------------------------------------------------

#[test]
fn string_equals_non_string_argument_deopts() {
    let _guard = deopt_lock();
    let f = compile_equals();
    let (a, _aa) = string_of("hello");
    // `other` has a different class id (a non-String object). The class-id
    // guard must trap so the interpreter's native equals returns false.
    let (bytes, coder) = encode("hello");
    let arr = make_byte_array(&bytes);
    let other = make_object(OTHER_CLASS_ID, arr.ptr(), coder, 0);
    let before = clear_deopt_signals();
    let r = f(a.ptr(), other.ptr());
    assert_eq!(
        r,
        i64::MIN,
        "non-String argument must return the deopt sentinel"
    );
    assert_one_deopt_after(before, "non-String equals argument");
}

#[test]
fn string_equals_coder_mismatch_deopts() {
    let _guard = deopt_lock();
    let f = compile_equals();
    // Two Strings with different `coder` values must trap (the inline byte
    // compare is only valid for matching coders; native equals decodes).
    let (a, _aa) = string_of("hi"); // LATIN1, coder 0
    let (bytes, _coder) = encode("hi");
    let arr = make_byte_array(&bytes);
    let b = make_object(STRING_CLASS_ID, arr.ptr(), 1, 0); // coder forced to 1
    let before = clear_deopt_signals();
    let r = f(a.ptr(), b.ptr());
    assert_eq!(r, i64::MIN, "coder mismatch must return the deopt sentinel");
    assert_one_deopt_after(before, "equals coder mismatch");
}
