// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The STRING_ACCESS intrinsics under **compressed oops**.
//!
//! Hole 1 of `gc/src/compressed_oops.rs`'s "two correctness holes":
//! `emit_load_string_value_ptr` emitted an unconditional 64-bit load of the
//! `String.value` reference field, so under narrow oops it read 4 bytes of
//! narrow oop plus 4 bytes of the adjacent `coder` and dereferenced the
//! result — a deterministic wild pointer. The stopgap was to refuse every
//! String intrinsic while narrow oops were on; the fix is the narrow arm
//! `emit_load_narrow_ref_field`, selected by
//! `StringFieldLayout::value_compact_is_narrow`.
//!
//! `cratonvm_types::narrow_oop`'s configuration is **process-wide and
//! write-once**, so this lives in its own integration-test binary (one process
//! per file) and does all its work in a single `#[test]`. Do not add a second
//! test function here that expects narrow oops to be off.

use cratonvm_jit::x64::compile;
use cratonvm_jit::{try_resolve_string_intrinsic, JitDirectCall, StringFieldLayout};
use cratonvm_jit_api::JitRuntimeHelpers;
use cratonvm_types::narrow_oop;
use cratonvm_types::{ArrayElementType, ClassId, ObjectHeader, ObjectKind, HEADER_SIZE};
use std::collections::{HashMap, HashSet};

unsafe extern "C" fn noop_uncommon_trap(_vm: i64, _reason: i64, _bci: i64) -> i64 {
    0
}

fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("narrow-oop String intrinsic test invoked an unwired runtime helper");
    }
    let s = stub as *const () as usize;
    unsafe extern "C" fn record_throw_bci(_bci: i64) {}
    unsafe extern "C" fn deopt_unserviceable_stub(
        _vm: i64,
        _info: i64,
        _args: i64,
        _n: i64,
    ) -> i64 {
        i64::MIN
    }
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
        throw_arithmetic: s,
        invoke_dispatch: s,
        invoke_virtual_mic: s,
        lambda_int_to_double: s,
        write_barrier: s,
        satb_pre_write_barrier: s,
        uncommon_trap: noop_uncommon_trap as *const () as usize,
        math_fma_double: s,
        math_fma_float: s,
        tlab_cursor_offset_in_thread: 0,
        tlab_end_offset_in_thread: 8,
        class_id_offset_in_obj: 0,
        jit_npe_with_action: s,
        dispatch_threw: s,
        jit_frem: s,
        jit_drem: s,
        ldc_string: s,
        set_throw_bci: record_throw_bci as *const () as usize,
        service_callee_deopt: deopt_unserviceable_stub as *const () as usize,
        ..Default::default()
    }
}

/// A heap object backed by a `Vec<u64>`, so the base address is 8-byte aligned.
/// Bump-allocate `bytes` from the ONE fixture arena, 8-aligned and zeroed.
///
/// # Why an arena and not a `Vec` per object
///
/// Each `FakeObj` used to own a separate `Vec<u64>`, which made the addresses
/// of the four fixtures four independent malloc results — and glibc is free to
/// serve those from DIFFERENT arenas. Measured 2026-09-08 on Linux/glibc, in a
/// plain `cargo test -p cratonvm-jit`:
///
/// ```text
/// latin1    = 0x73af1c000b80   <- thread arena (mmap)
/// empty_arr = 0x5adff817b1b0   <- main heap (brk), and the minimum
/// s         = 0x73af1c000d00
/// empty     = 0x73af1c000d30
/// ```
///
/// `heap_base` is derived from the minimum, so the window started at the main
/// heap and the other three sat ~33 TB above it — against a narrow window that
/// is 32 GiB wide (`(u32::MAX << 3)`). `is_encodable` correctly said no, and
/// the test failed 100% of the time on Linux while passing on Windows, whose
/// allocator happened to keep all four together.
///
/// The 2026-08-13 fix addressed the allocation ORDER (allocate everything,
/// then derive the base from the minimum) and that reasoning still holds. It
/// could not fix the SPAN, because no ordering makes independent mallocs land
/// near each other. One arena does: every fixture is now carved from a single
/// 64 KiB block, so the four are a few hundred bytes apart by construction and
/// the geometry stops depending on the allocator at all.
///
/// Leaked on purpose. These fixtures are handed to compiled code as raw
/// pointers and must outlive every call in the test; a leak in a test binary
/// that exits immediately afterwards costs nothing and removes any question of
/// a fixture being freed while a JIT artifact still names it.
fn arena_alloc(bytes: usize) -> *mut u8 {
    const ARENA_BYTES: usize = 64 * 1024;
    static ARENA: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    static USED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    // `Vec<u64>` so the block is 8-aligned; every offset handed out is a
    // multiple of 8, so every object is too.
    let base = *ARENA.get_or_init(|| {
        let words = vec![0u64; ARENA_BYTES / 8];
        Box::leak(words.into_boxed_slice()).as_mut_ptr() as usize
    });
    let want = bytes.div_ceil(8).max(1) * 8;
    let off = USED.fetch_add(want, std::sync::atomic::Ordering::Relaxed);
    assert!(
        off + want <= ARENA_BYTES,
        "fixture arena exhausted ({off} + {want} > {ARENA_BYTES}); raise ARENA_BYTES"
    );
    (base + off) as *mut u8
}

struct FakeObj {
    base: *mut u8,
}

impl FakeObj {
    fn with_bytes(total: usize) -> Self {
        FakeObj {
            base: arena_alloc(total),
        }
    }
    fn base(&mut self) -> *mut u8 {
        self.base
    }
    fn ptr(&self) -> i64 {
        self.base as i64
    }
}

fn make_byte_array(data: &[u8]) -> FakeObj {
    let mut obj = FakeObj::with_bytes(HEADER_SIZE + data.len());
    let base = obj.base();
    unsafe {
        // Through the constructor, not at literal offsets: the 2026-08-07
        // `header-16` shrink moved `shape` to 4..8 (it IS the array length) and
        // packed `kind`/`element_type` into one byte of the mark word, so the
        // old offset-4/5/12 writes set the length to `ObjectKind::Array as u8`
        // == 1 and left the element type unset.
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
        std::ptr::copy_nonoverlapping(data.as_ptr(), base.add(HEADER_SIZE), data.len());
    }
    obj
}

/// Dense-registry-safe class id — `register_class_layout` indexes a dense
/// `Vec` by class id.
const NARROW_STRING_CLASS_ID: u32 = 7;

/// The real JDK25 `java/lang/String` compact layout **at narrow widths**:
/// `value:[B` (4 bytes) at 0, `coder:B` at 4, `hash:I` at 8, `hashIsZero:Z` at
/// 12 — a 16-byte body where the wide layout needs 24.
fn register_narrow_string_layout() -> u32 {
    use cratonvm_types::{register_class_layout, CompactLayout, FieldStorageKind};
    register_class_layout(
        cratonvm_types::FIRST_LAYOUT_DOMAIN,
        NARROW_STRING_CLASS_ID,
        std::sync::Arc::new(CompactLayout {
            field_offsets: vec![0, 4, 8, 12],
            is_ref: vec![true, false, false, false],
            field_kinds: vec![
                FieldStorageKind::Reference,
                FieldStorageKind::Byte,
                FieldStorageKind::Int,
                FieldStorageKind::Boolean,
            ],
            ref_offsets: vec![0],
            body_size: 16,
        }),
    );
    NARROW_STRING_CLASS_ID
}

/// The storage for a COMPACT `java/lang/String`, with nothing written yet.
///
/// Split from [`fill_narrow_string`] on purpose: filling one ENCODES the
/// `value` pointer, and encoding is only legal — and only correct — once
/// `narrow_oop::enable` has run with a base that already covers every address
/// this test will ever hand it. Allocating and filling in one step made the
/// test depend on malloc ordering; see the ALLOCATE-THEN-ENABLE comment in
/// `string_intrinsics_decode_a_narrow_value_slot`.
fn alloc_narrow_string() -> FakeObj {
    FakeObj::with_bytes(HEADER_SIZE + 16)
}

/// Write a COMPACT `java/lang/String` whose `value` slot holds a **narrow** oop
/// into storage from [`alloc_narrow_string`]. Must run AFTER
/// `narrow_oop::enable`.
fn fill_narrow_string(obj: &mut FakeObj, value_ptr: i64, coder: u8, hash: i32) {
    let base = obj.base();
    let narrow = narrow_oop::encode(value_ptr as u64);
    unsafe {
        std::ptr::copy_nonoverlapping(NARROW_STRING_CLASS_ID.to_le_bytes().as_ptr(), base, 4);
        *base.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET) = ObjectKind::Object as u8;
        *base.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) = cratonvm_types::GC_FLAG_COMPACT;
        std::ptr::copy_nonoverlapping(
            4u32.to_le_bytes().as_ptr(),
            base.add(cratonvm_types::NUM_SLOTS_OFFSET),
            4,
        );
        let body = base.add(HEADER_SIZE);
        std::ptr::copy_nonoverlapping(narrow.to_le_bytes().as_ptr(), body, 4);
        *body.add(4) = coder;
        std::ptr::copy_nonoverlapping(hash.to_le_bytes().as_ptr(), body.add(8), 4);
        *body.add(12) = 0;
    }
}

/// Compile `return this.<name>()` for a no-arg int-returning String intrinsic.
fn compile_unary(layout: StringFieldLayout, name: &str, descriptor: &str) -> impl Fn(i64) -> i64 {
    let entry = try_resolve_string_intrinsic("java/lang/String", name, descriptor, Some(layout))
        .unwrap_or_else(|| {
            panic!("{name}{descriptor} must still register under narrow oops — the blanket refusal is gone")
        })
        .0;
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
    )
    .unwrap_or_else(|| panic!("{name}{descriptor} failed to compile under narrow oops"));
    move |this: i64| unsafe { compiled.try_call(&[this]).expect("test JIT call") }
}

fn java_hash(s: &str) -> i32 {
    s.bytes()
        .fold(0i32, |h, c| h.wrapping_mul(31).wrapping_add(c as i32))
}

#[test]
fn string_intrinsics_decode_a_narrow_value_slot() {
    // ALLOCATE, THEN ENABLE — in that order, and not the other way round.
    //
    // Every `FakeObj` is carved from one arena (see `arena_alloc`), but the
    // allocator is still free to hand them out in ANY address order. This test used to allocate the
    // first backing array, derive `heap_base` from THAT one alone, enable
    // narrow oops, and only then allocate the rest — so any later allocation
    // landing at a lower address was below the base and
    // `narrow_oop::encode`'s guard aborted the process:
    //
    //     cratonvm: FATAL narrow-oop encode failure: address 0x… outside
    //     [0x…, 0x…) shift=3 - compressed oops geometry is wrong
    //
    // That is a property of malloc, not of anything under test. Measured
    // 2026-08-13 on an unmodified tree: the built test exe run directly failed
    // **23 of 25** times, while the same test through `cargo test` failed 0 of
    // 6 — environment-sensitive, which is why it stayed green in the usual CI
    // shape and then took down a full `cargo test -p cratonvm-jit` at random.
    //
    // So: allocate EVERY object first, take the minimum base across all of
    // them, and derive the window from that. `min - 8` (not `min`) because
    // `encode` rejects `delta == 0` — narrow oop 0 is null — so the lowest
    // object has to sit one shift-unit above the base, exactly as the original
    // `value_ptr - 8` intended for its single object.
    let latin1 = make_byte_array(b"hello world");
    let empty_arr = make_byte_array(b"");
    let mut s = alloc_narrow_string();
    let mut empty = alloc_narrow_string();

    let value_ptr = latin1.ptr();
    let empty_ptr = empty_arr.ptr();
    for (what, p) in [
        ("latin1", value_ptr),
        ("empty_arr", empty_ptr),
        ("s", s.ptr()),
        ("empty", empty.ptr()),
    ] {
        assert_eq!(p % 8, 0, "{what}: arena offsets are multiples of 8");
    }
    // Only the two backing ARRAYS are ever encoded (they are what a `value`
    // slot points at); the String objects themselves are passed to compiled
    // code as raw receiver pointers. They are folded into the minimum anyway —
    // it costs nothing, the window is 32 GiB wide, and it means a future
    // assertion that encodes one cannot resurrect this bug.
    let heap_base = [value_ptr, empty_ptr, s.ptr(), empty.ptr()]
        .into_iter()
        .min()
        .expect("four addresses") as u64
        - 8;
    assert!(
        narrow_oop::enable(heap_base, 3),
        "narrow-oop geometry must be accepted"
    );
    assert!(narrow_oop::narrow_oops_enabled());
    // The invariant the ALLOCATE-THEN-ENABLE order exists to establish, stated
    // where a future edit will trip over it. Without this, adding a fifth
    // object — or getting the minimum wrong — reverts to the old failure mode:
    // a process abort on 23 runs in 25 and a clean pass on the other 2, which
    // is the hardest possible thing to attribute. `is_encodable` also rejects
    // `addr == base`, so it proves the `- 8` headroom too, not just the order.
    for (what, p) in [
        ("latin1", value_ptr),
        ("empty_arr", empty_ptr),
        ("s", s.ptr()),
        ("empty", empty.ptr()),
    ] {
        assert!(
            narrow_oop::is_encodable(p as u64),
            "{what} at {p:#x} is outside the narrow window derived from these same four addresses"
        );
    }
    assert_eq!(
        narrow_oop::ref_field_size(),
        4,
        "a ref field is now 4 bytes"
    );
    assert_eq!(
        narrow_oop::decode(narrow_oop::encode(value_ptr as u64)),
        value_ptr as u64
    );

    let layout = StringFieldLayout::new(0, Some(1), 2, register_narrow_string_layout());
    assert_eq!(
        layout.value_compact_offset, HEADER_SIZE as i32,
        "compact offsets are exact payload addresses"
    );
    assert!(
        layout.value_compact_is_narrow,
        "a registered narrow ref field must select the narrow load arm",
    );

    fill_narrow_string(&mut s, value_ptr, 0, 0);
    let s_ptr = s.ptr();

    // `length()` = value.length >> coder — it dereferences the decoded slot to
    // read the array header. A wide load here reads `coder`/`hash` as the top
    // half of the pointer and faults.
    let length = compile_unary(layout, "length", "()I");
    assert_eq!(length(s_ptr), 11, "length over a narrow value slot");

    let is_empty = compile_unary(layout, "isEmpty", "()Z");
    assert_eq!(is_empty(s_ptr), 0, "isEmpty over a narrow value slot");

    // `hashCode()` walks the array *elements*, so it proves the decoded base is
    // the real array and not merely a readable address.
    let hash_code = compile_unary(layout, "hashCode", "()I");
    assert_eq!(
        hash_code(s_ptr) as i32,
        java_hash("hello world"),
        "hashCode over a narrow value slot",
    );

    // An empty string: `value.length == 0`, and the narrow slot is still a
    // real (non-null) array reference. Both halves were allocated up front;
    // only the fill happens here.
    fill_narrow_string(&mut empty, empty_ptr, 0, 0);
    assert_eq!(length(empty.ptr()), 0, "empty length");
    assert_eq!(is_empty(empty.ptr()), 1, "empty isEmpty");

    // No keep-alive is needed, and the four `drop(..)` calls that used to stand
    // here are gone with the `Vec<u64>` they were written for. `FakeObj` is one
    // raw pointer into the arena `arena_alloc` LEAKS on purpose (see its doc),
    // so there is nothing for a drop to free and nothing a compiler could free
    // early. Since `534e1921b` clippy said so too — `drop_non_drop`, four
    // errors, which is a `-D warnings` failure on a step that runs BEFORE
    // `cargo test --workspace` in `ci.yml` and therefore takes the whole test
    // gate with it.
}
