// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Differential tests for the CRC32 / CRC32C JIT `update` call-site
//! intrinsic family (Phase 4c).
//!
//! The intrinsics inline `java.util.zip.CRC32.update` and
//! `java.util.zip.CRC32C.update` — both the single-byte `update(I)V` and the
//! ranged `update([BII)V` overloads — directly into the JITted code with no
//! `CALL`. CRC32C folds bytes with the hardware `CRC32` instruction (which
//! computes exactly the Castagnoli polynomial); CRC32 folds with an inline
//! reflected-CRC bit loop (IEEE 802.3 poly 0xEDB88320 — the hardware
//! instruction is the wrong polynomial).
//!
//! Each test JIT-compiles a tiny synthetic method, e.g.
//!
//!     void f(CRC32C recv, int b) { recv.update(b); }
//!
//! builds a heap-layout receiver object in Rust-owned memory (an object
//! header carrying the class id, followed by one 16-byte `Value` field cell
//! for the single `int crc` instance field at slot 0), runs the JITted code,
//! and asserts the running CRC state written back to field slot 0 is
//! bit-identical to a reference reflected-CRC computation.
//!
//! ## Differential oracle
//!
//! The reference functions [`ref_crc32c_step`] / [`ref_crc32_step`] below are
//! verbatim copies of the native CRC implementations
//! (`native-builtins/src/zip_crc32c.rs::crc32c_step` and
//! `native-builtins/src/zip_real.rs::crc32_step`) — the bit-exact oracle the
//! JIT must match. Each test additionally asserts the canonical published
//! check value (CRC-32C of `"123456789"` = `0xE3069283`; CRC-32 of
//! `"123456789"` = `0xCBF43926`), so a divergence in either the reference or
//! the JIT is caught even if both drifted together.
//!
//! ## Receiver layout
//!
//! `java.util.zip.CRC32` and `CRC32C` each declare exactly one instance
//! field — `private int crc` at slot 0 — holding the running (uncomplemented)
//! CRC state (see gaps/crc_layout_contract.md). The instance field
//! cell is the 16-byte `Value` enum at `HEADER_SIZE + 0*SLOT_SIZE`; the `Int`
//! tag word sits at `FIELD_CELL_TAG_OFFSET`, the 32-bit payload at
//! `FIELD_CELL_PAYLOAD32_OFFSET`. We synthesize exactly that layout and hand
//! the JIT a raw pointer, so the inline code mutates our buffer directly.

use cratonvm_jit::x64::compile;
use cratonvm_jit::JitDirectCall;
use cratonvm_jit_api::JitRuntimeHelpers;
use cratonvm_types::{
    ARRAY_LENGTH_OFFSET, FIELD_CELL_PAYLOAD32_OFFSET, FIELD_CELL_TAG_OFFSET, HEADER_SIZE, SLOT_SIZE,
};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

// ---------------------------------------------------------------------------
// Reference (oracle) CRC step functions — verbatim copies of the native impls.
// ---------------------------------------------------------------------------

/// Verbatim copy of `native-builtins/src/zip_crc32c.rs::crc32c_step`.
/// CRC-32C (Castagnoli), reflected poly 0x82F63B78. `crc` is the running
/// (uncomplemented) state.
fn ref_crc32c_step(mut crc: u32, data: &[u8]) -> u32 {
    const REVERSED_CRC32C_POLY: u32 = 0x82F6_3B78;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (REVERSED_CRC32C_POLY & mask);
        }
    }
    crc
}

/// Verbatim copy of `native-builtins/src/zip_real.rs::crc32_step`.
/// CRC-32/IEEE 802.3, reflected poly 0xEDB88320. `crc` is the running
/// (uncomplemented) state.
fn ref_crc32_step(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    crc
}

// ---------------------------------------------------------------------------
// Stub runtime helpers — a correctly-emitted CRC32 intrinsic invokes none of
// them (no helper CALL on the inlined fast path). Legacy deopt stubs call
// `uncommon_trap`; default real-frame deopt stubs stash a reconstructed frame.
// The helpers below accept either signal so the tests track the active deopt
// backend instead of a specific implementation detail.
// ---------------------------------------------------------------------------

/// Set by the `uncommon_trap` stub when a deopt edge is taken.
static DEOPT_TRIPPED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn uncommon_trap_stub(_vm: i64, _reason: i64, _bci: i64) {
    DEOPT_TRIPPED.store(true, Ordering::SeqCst);
}

fn clear_deopt_signals() {
    DEOPT_TRIPPED.store(false, Ordering::SeqCst);
    let _ = cratonvm_jit::deopt::take_last_deopt();
}

fn assert_deopt_signaled(context: &str) {
    let legacy_trap = DEOPT_TRIPPED.load(Ordering::SeqCst);
    let real_frame = cratonvm_jit::deopt::take_last_deopt().is_some();
    assert!(legacy_trap || real_frame, "{context} must hit a deopt path");
}

fn assert_no_deopt_signaled(context: &str) {
    let legacy_trap = DEOPT_TRIPPED.load(Ordering::SeqCst);
    let real_frame = cratonvm_jit::deopt::take_last_deopt().is_some();
    assert!(
        !legacy_trap && !real_frame,
        "{context} must not hit a deopt path; legacy_trap={legacy_trap}, real_frame={real_frame}"
    );
}

fn stub_helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("CRC32 intrinsic test invoked an unwired runtime helper");
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
        uncommon_trap: uncommon_trap_stub as *const () as usize,
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

// ---------------------------------------------------------------------------
// Heap-layout fakes.
// ---------------------------------------------------------------------------

/// A CRC32/CRC32C receiver object: an object header (class id at offset 0)
/// followed by one 16-byte `Value` field cell for `int crc` at slot 0.
struct FakeReceiver {
    storage: Vec<u64>,
}

impl FakeReceiver {
    /// Build a receiver with the given `class_id` whose `int crc` field
    /// (slot 0) is initialised to the class-specific `crc` state used by the
    /// intrinsic test (public CRC32 value or CRC32C running state).
    fn new(class_id: u32, crc: u32) -> Self {
        let byte_len = HEADER_SIZE + SLOT_SIZE; // header + one field cell
        let words = byte_len.div_ceil(8).max(1);
        let mut storage = vec![0u64; words];
        let base = storage.as_mut_ptr() as *mut u8;
        // SAFETY: writes within the freshly allocated, uniquely owned buffer.
        unsafe {
            // Class id at ObjectHeader offset 0.
            (base as *mut u32).write_unaligned(class_id);
            // Field cell at HEADER_SIZE: Int tag (0) + 32-bit payload.
            let cell = base.add(HEADER_SIZE);
            (cell.add(FIELD_CELL_TAG_OFFSET) as *mut u32).write_unaligned(0);
            (cell.add(FIELD_CELL_PAYLOAD32_OFFSET) as *mut u32).write_unaligned(crc);
        }
        FakeReceiver { storage }
    }

    fn ptr(&self) -> *mut u8 {
        self.storage.as_ptr() as *mut u8
    }

    /// Read back the CRC state from field slot 0.
    fn crc(&self) -> u32 {
        // SAFETY: the cell payload is within the allocation.
        unsafe {
            let cell = self.ptr().add(HEADER_SIZE);
            (cell.add(FIELD_CELL_PAYLOAD32_OFFSET) as *const u32).read_unaligned()
        }
    }

    /// Read back the field-cell tag word — must stay `Int` (0) after a write.
    fn tag(&self) -> u32 {
        // SAFETY: the cell tag is within the allocation.
        unsafe {
            let cell = self.ptr().add(HEADER_SIZE);
            (cell.add(FIELD_CELL_TAG_OFFSET) as *const u32).read_unaligned()
        }
    }
}

/// A heap-layout `byte[]`: object header with the element count at
/// `ARRAY_LENGTH_OFFSET`, packed byte data from `HEADER_SIZE`.
struct FakeByteArray {
    storage: Vec<u64>,
}

impl FakeByteArray {
    fn new(data: &[u8]) -> Self {
        let byte_len = HEADER_SIZE + data.len();
        let words = byte_len.div_ceil(8).max(1);
        let mut storage = vec![0u64; words];
        let base = storage.as_mut_ptr() as *mut u8;
        // SAFETY: writes within the freshly allocated, uniquely owned buffer.
        unsafe {
            (base.add(ARRAY_LENGTH_OFFSET) as *mut u32).write_unaligned(data.len() as u32);
            for (i, &b) in data.iter().enumerate() {
                base.add(HEADER_SIZE + i).write(b);
            }
        }
        FakeByteArray { storage }
    }

    fn ptr(&self) -> *mut u8 {
        self.storage.as_ptr() as *mut u8
    }
}

// ---------------------------------------------------------------------------
// JIT-compile helpers.
// ---------------------------------------------------------------------------

/// The synthetic class id used for the CRC receiver in every test. Any
/// non-zero value works — the codegen guard compares the receiver's header
/// word against `guard_class_id`, which we set to this same constant.
const CRC_CLASS_ID: u32 = 0x00C2_C3C4;

/// JIT-compile `void f(recv, int b) { recv.update(b); }`.
///
/// Bytecode: `aload_0` (0x2a), `iload_1` (0x1b), `invokevirtual #1`
/// (0xb6 0x00 0x01), `return` (0xb1). The `invokevirtual` is at pc 2 — the
/// `direct_calls` key. JLS param count is 1 (the int); the codegen ladder
/// adds the receiver back.
fn compile_update_byte(entry: usize, guard_class_id: u32) -> impl Fn(*mut u8, i32) {
    let code: Vec<u8> = vec![0x2a, 0x1b, 0xb6, 0x00, 0x01, 0xb1, 0, 0];
    let compiled = compile(
        &code,
        6,
        2, // num_params: receiver + int
        2, // max_locals
        true,
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
                return_type: b'V',
                guard_class_id,
            },
        )],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &stub_helpers(),
        HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("JIT compilation of CRC32.update(I)V intrinsic failed");
    move |recv: *mut u8, b: i32| {
        // SAFETY: `compiled` is valid JITted code; `recv` points at a live
        // heap-layout receiver owned by the caller. `needs_heap` is true, so
        // the first C argument is the heap pointer (unused by the intrinsic;
        // 0 is fine — no helper is called).
        unsafe {
            compiled
                .try_call(&[0, recv as i64, b as i64])
                .expect("test JIT call");
        }
    }
}

/// JIT-compile `void f(recv, byte[] arr, int off, int len) {
/// recv.update(arr, off, len); }`.
///
/// Bytecode: `aload_0`, `aload_1` (0x2b), `iload_2` (0x1c), `iload_3`
/// (0x1d), `invokevirtual #1`, `return`. The `invokevirtual` is at pc 4.
fn compile_update_bytes(entry: usize, guard_class_id: u32) -> impl Fn(*mut u8, *mut u8, i32, i32) {
    let code: Vec<u8> = vec![0x2a, 0x2b, 0x1c, 0x1d, 0xb6, 0x00, 0x01, 0xb1, 0, 0];
    let compiled = compile(
        &code,
        8,
        4, // num_params: receiver + arr + off + len
        4, // max_locals
        true,
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
                num_params: 3,
                return_type: b'V',
                guard_class_id,
            },
        )],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &stub_helpers(),
        HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("JIT compilation of CRC32.update([BII)V intrinsic failed");
    move |recv: *mut u8, arr: *mut u8, off: i32, len: i32| {
        // SAFETY: see `compile_update_byte`.
        unsafe {
            compiled
                .try_call(&[0, recv as i64, arr as i64, off as i64, len as i64])
                .expect("test JIT call");
        }
    }
}

/// Resolve a CRC32/CRC32C `update` overload to its intrinsic entry sentinel.
fn resolve(class: &str, descriptor: &str) -> Option<usize> {
    cratonvm_jit::try_resolve_intrinsic(class, "update", descriptor).map(|(entry, _, _)| entry)
}

// ---------------------------------------------------------------------------
// CRC32C tests — hardware Castagnoli fold.
// ---------------------------------------------------------------------------

#[test]
fn crc32c_intrinsics_register_when_supported() {
    // CRC32C.update(I)V and update([BII)V resolve to an intrinsic exactly
    // when the host has SSE4.2 (the hardware CRC32 instruction). On a host
    // without SSE4.2 they fall back to native dispatch (None).
    let has = cratonvm_jit::x64::has_sse42();
    assert_eq!(
        resolve("java/util/zip/CRC32C", "(I)V").is_some(),
        has,
        "CRC32C.update(I)V registration must track has_sse42()",
    );
    assert_eq!(
        resolve("java/util/zip/CRC32C", "([BII)V").is_some(),
        has,
        "CRC32C.update([BII)V registration must track has_sse42()",
    );
    // The whole-array overload is intentionally never registered.
    assert_eq!(resolve("java/util/zip/CRC32C", "([B)V"), None);
}

#[test]
fn crc32c_update_byte_matches_oracle() {
    let Some(entry) = resolve("java/util/zip/CRC32C", "(I)V") else {
        eprintln!("SSE4.2 unavailable — CRC32C.update(I)V intrinsic skipped");
        return;
    };
    let f = compile_update_byte(entry, CRC_CLASS_ID);

    // Fold "123456789" one byte at a time; the running state starts at
    // 0xFFFFFFFF (what <init>/reset sets). The published CRC-32C check value
    // is the *complement* of the final running state.
    let data = b"123456789";
    let recv = FakeReceiver::new(CRC_CLASS_ID, 0xFFFF_FFFF);
    let mut oracle = 0xFFFF_FFFFu32;
    for &b in data {
        f(recv.ptr(), b as i32);
        oracle = ref_crc32c_step(oracle, &[b]);
        assert_eq!(recv.crc(), oracle, "CRC32C running state diverged");
    }
    assert_eq!(recv.tag(), 0, "field cell tag must stay Int after write");
    assert_eq!(
        !recv.crc(),
        0xE306_9283,
        "CRC-32C of \"123456789\" must be the canonical 0xE3069283",
    );

    // The byte argument is masked to 8 bits: update(0x7F00 | b) == update(b).
    let r1 = FakeReceiver::new(CRC_CLASS_ID, 0xFFFF_FFFF);
    let r2 = FakeReceiver::new(CRC_CLASS_ID, 0xFFFF_FFFF);
    f(r1.ptr(), 0x41); // 'A'
    f(r2.ptr(), 0x41 | 0x7F00); // high bits must be ignored
    assert_eq!(
        r1.crc(),
        r2.crc(),
        "update(I) must fold only the low 8 bits"
    );
}

#[test]
fn crc32c_update_bytes_matches_oracle() {
    let Some(entry) = resolve("java/util/zip/CRC32C", "([BII)V") else {
        eprintln!("SSE4.2 unavailable — CRC32C.update([BII)V intrinsic skipped");
        return;
    };
    let f = compile_update_bytes(entry, CRC_CLASS_ID);

    // Whole-array, empty range, sub-range, and the RFC 3720 zero/0xFF blocks.
    let cases: Vec<Vec<u8>> = vec![
        b"123456789".to_vec(),
        Vec::new(),
        b"The quick brown fox jumps over the lazy dog".to_vec(),
        vec![0u8; 32],
        vec![0xFFu8; 32],
        (0..=255u16).map(|x| x as u8).collect(),
    ];
    for data in &cases {
        let arr = FakeByteArray::new(data);
        let recv = FakeReceiver::new(CRC_CLASS_ID, 0xFFFF_FFFF);
        f(recv.ptr(), arr.ptr(), 0, data.len() as i32);
        let oracle = ref_crc32c_step(0xFFFF_FFFF, data);
        assert_eq!(
            recv.crc(),
            oracle,
            "CRC32C.update([BII) full-array mismatch"
        );
    }

    // Sub-range: fold bytes [3, 3+5) of a longer array.
    let data = b"0123456789abcdef";
    let arr = FakeByteArray::new(data);
    let recv = FakeReceiver::new(CRC_CLASS_ID, 0xFFFF_FFFF);
    f(recv.ptr(), arr.ptr(), 3, 5);
    assert_eq!(
        recv.crc(),
        ref_crc32c_step(0xFFFF_FFFF, &data[3..8]),
        "CRC32C.update([BII) sub-range mismatch",
    );

    // Incremental folding across two update() calls equals a single update().
    let full = b"differential-crc32c-check";
    let (a, b) = full.split_at(11);
    let arr_full = FakeByteArray::new(full);
    let arr_a = FakeByteArray::new(a);
    let arr_b = FakeByteArray::new(b);
    let r_one = FakeReceiver::new(CRC_CLASS_ID, 0xFFFF_FFFF);
    let r_two = FakeReceiver::new(CRC_CLASS_ID, 0xFFFF_FFFF);
    f(r_one.ptr(), arr_full.ptr(), 0, full.len() as i32);
    f(r_two.ptr(), arr_a.ptr(), 0, a.len() as i32);
    f(r_two.ptr(), arr_b.ptr(), 0, b.len() as i32);
    assert_eq!(r_one.crc(), r_two.crc(), "CRC32C incremental != one-shot");
}

// ---------------------------------------------------------------------------
// CRC32 (IEEE) tests — inline reflected-CRC bit loop.
// ---------------------------------------------------------------------------

#[test]
fn crc32_ieee_intrinsics_are_disabled_for_public_state_fidelity() {
    // Real JDK CRC32 uses public state whereas CRC32C uses a complemented
    // running state. The native CRC32 path is bit-exact and avoids a corrupt
    // stored-entry checksum on compiled archive-writing call shapes.
    assert_eq!(resolve("java/util/zip/CRC32", "(I)V"), None);
    assert_eq!(resolve("java/util/zip/CRC32", "([BII)V"), None);
    assert_eq!(resolve("java/util/zip/CRC32", "([B)V"), None);
}

#[test]
fn crc32_ieee_update_byte_matches_oracle() {
    let Some(entry) = resolve("java/util/zip/CRC32", "(I)V") else {
        return;
    };
    let f = compile_update_byte(entry, CRC_CLASS_ID);

    let data = b"123456789";
    // Real JDK CRC32 stores the public CRC value (fresh = 0), unlike CRC32C
    // which stores the complemented running state. The JIT must preserve that
    // public-state contract around its reflected inner fold.
    let recv = FakeReceiver::new(CRC_CLASS_ID, 0);
    let mut oracle = 0u32;
    for &b in data {
        f(recv.ptr(), b as i32);
        oracle = !ref_crc32_step(!oracle, &[b]);
        assert_eq!(recv.crc(), oracle, "CRC32 public state diverged");
    }
    assert_eq!(recv.tag(), 0, "field cell tag must stay Int after write");
    assert_eq!(
        recv.crc(),
        0xCBF4_3926,
        "CRC-32 of \"123456789\" must be the canonical 0xCBF43926",
    );

    // Negative ints and high bits: only the low 8 bits are folded.
    let r1 = FakeReceiver::new(CRC_CLASS_ID, 0);
    let r2 = FakeReceiver::new(CRC_CLASS_ID, 0);
    f(r1.ptr(), 0xAB);
    f(r2.ptr(), (-1i32 & !0xFF) | 0xAB); // 0xFFFFFFAB — same low byte
    assert_eq!(
        r1.crc(),
        r2.crc(),
        "update(I) must fold only the low 8 bits"
    );
}

#[test]
fn crc32_ieee_update_bytes_matches_oracle() {
    let Some(entry) = resolve("java/util/zip/CRC32", "([BII)V") else {
        return;
    };
    let f = compile_update_bytes(entry, CRC_CLASS_ID);

    let cases: Vec<Vec<u8>> = vec![
        b"123456789".to_vec(),
        Vec::new(),
        b"The quick brown fox jumps over the lazy dog".to_vec(),
        vec![0u8; 40],
        vec![0xFFu8; 17],
        (0..=255u16).map(|x| x as u8).collect(),
    ];
    for data in &cases {
        let arr = FakeByteArray::new(data);
        let recv = FakeReceiver::new(CRC_CLASS_ID, 0);
        f(recv.ptr(), arr.ptr(), 0, data.len() as i32);
        assert_eq!(
            recv.crc(),
            !ref_crc32_step(!0, data),
            "CRC32.update([BII) full-array mismatch",
        );
    }

    // Sub-range fold.
    let data = b"0123456789abcdef";
    let arr = FakeByteArray::new(data);
    let recv = FakeReceiver::new(CRC_CLASS_ID, 0);
    f(recv.ptr(), arr.ptr(), 4, 7);
    assert_eq!(
        recv.crc(),
        !ref_crc32_step(!0, &data[4..11]),
        "CRC32.update([BII) sub-range mismatch",
    );
}

// ---------------------------------------------------------------------------
// Guard / fallback tests — these MUST deopt rather than corrupt state.
// ---------------------------------------------------------------------------

#[test]
fn class_id_mismatch_deopts() {
    // A receiver whose dynamic class id is NOT the guarded class (e.g. a
    // hypothetical CRC32 subclass that overrides update) must take the deopt
    // edge — the running crc field must be left untouched.
    let Some(entry) = resolve("java/util/zip/CRC32", "(I)V") else {
        return;
    };
    let f = compile_update_byte(entry, CRC_CLASS_ID);

    clear_deopt_signals();
    let wrong = FakeReceiver::new(CRC_CLASS_ID ^ 0x1, 0xFFFF_FFFF);
    f(wrong.ptr(), b'x' as i32);
    assert_deopt_signaled("class-id mismatch");
    assert_eq!(
        wrong.crc(),
        0xFFFF_FFFF,
        "deopt must NOT mutate the receiver's crc field",
    );
}

#[test]
fn null_array_deopts() {
    // update([BII)V with a null array must deopt (the interpreter then
    // re-runs and the native override raises NullPointerException).
    let Some(entry) = resolve("java/util/zip/CRC32", "([BII)V") else {
        return;
    };
    let f = compile_update_bytes(entry, CRC_CLASS_ID);

    clear_deopt_signals();
    let recv = FakeReceiver::new(CRC_CLASS_ID, 0xFFFF_FFFF);
    f(recv.ptr(), std::ptr::null_mut(), 0, 4);
    assert_deopt_signaled("null array");
    assert_eq!(recv.crc(), 0xFFFF_FFFF, "deopt must not mutate crc");
}

#[test]
fn out_of_bounds_range_deopts() {
    // off/len outside [0, array.length] must deopt (preserving AIOOBE).
    let Some(entry) = resolve("java/util/zip/CRC32", "([BII)V") else {
        return;
    };
    let f = compile_update_bytes(entry, CRC_CLASS_ID);
    let data = b"0123456789";

    for &(off, len) in &[(0i32, 11i32), (8, 5), (-1, 2), (3, -1), (10, 1)] {
        clear_deopt_signals();
        let arr = FakeByteArray::new(data);
        let recv = FakeReceiver::new(CRC_CLASS_ID, 0xFFFF_FFFF);
        f(recv.ptr(), arr.ptr(), off, len);
        assert_deopt_signaled(&format!("out-of-bounds (off={off}, len={len})"));
        assert_eq!(recv.crc(), 0xFFFF_FFFF, "deopt must not mutate crc");
    }

    // A null receiver also deopts (the interpreter NPEs on virtual dispatch).
    clear_deopt_signals();
    let arr = FakeByteArray::new(data);
    f(std::ptr::null_mut(), arr.ptr(), 0, 4);
    assert_deopt_signaled("null receiver");
}

#[test]
fn empty_range_is_a_noop() {
    // update([BII)V with len == 0 folds nothing — the crc is unchanged and
    // no deopt is taken (the range [off, off) is in bounds for any valid off).
    for class in ["java/util/zip/CRC32", "java/util/zip/CRC32C"] {
        let Some(entry) = resolve(class, "([BII)V") else {
            continue; // CRC32C skipped without SSE4.2
        };
        let f = compile_update_bytes(entry, CRC_CLASS_ID);
        clear_deopt_signals();
        let arr = FakeByteArray::new(b"payload");
        let recv = FakeReceiver::new(CRC_CLASS_ID, 0x1234_5678);
        f(recv.ptr(), arr.ptr(), 4, 0);
        assert_no_deopt_signaled("empty range");
        assert_eq!(
            recv.crc(),
            0x1234_5678,
            "empty range must leave crc unchanged"
        );
    }
}
