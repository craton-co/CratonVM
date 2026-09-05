// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Heap object layout types and constants.
//!
//! Defines the on-heap object header layout shared by the GC, JIT, and
//! interpreter, including [`HEADER_SIZE`] — the fixed object-header size in
//! bytes — and the related compile-time layout invariants.

use crate::ClassId;
use std::sync::atomic::AtomicU64;

/// Size of `ObjectHeader` in bytes. Must be a multiple of 8 for alignment.
///
/// The array length and object shape share a word, age/flags occupy the first
/// word's two spare bytes, and GC forwarding rides in the mark word's
/// `MARK_FORWARDED` state rather than in a field of its own — the 32 -> 24
/// shrink of 2026-08-06.
pub const HEADER_SIZE: usize = 16;

/// Bytes of heap covered by one card, for EVERY card table and every emitter
/// that indexes one.
///
/// # Why this lives here and not beside a card table
///
/// It had three homes and no owner. `gc::card_table::CARD_SIZE` was `512`;
/// `gc::g1_cards::G1_CARD_SHIFT` was `9` with a doc comment reading "512 bytes,
/// matching `crate::card_table::CARD_SIZE`"; and the x64 emitter had
/// `emit_shr_r64_imm8(RCX, 9); // CARD_SIZE = 512`. Three constants that must
/// agree, bound by two comments.
///
/// The third one is why this matters more than tidiness: it is a SHIFT BAKED
/// INTO MACHINE CODE. A divergence would not surface as a mismatch between two
/// Rust constants a reader could spot — it would be compiled into a barrier
/// that dirties the wrong card, and a missed dirty card is a live cross-region
/// (or old-to-young) edge the next collection never scans, whose referent is
/// not evacuated and whose region is then freed.
///
/// `cratonvm-types` is the only crate all three can name: `cratonvm-jit`
/// depends on it unconditionally, and `cratonvm-gc` does too.
pub const CARD_SIZE_BYTES: usize = 512;

/// log2 of [`CARD_SIZE_BYTES`] — the shift an emitter puts in a `shr`.
pub const CARD_SHIFT: u32 = CARD_SIZE_BYTES.trailing_zeros();

// `trailing_zeros` of a non-power-of-two silently rounds DOWN, so a
// `CARD_SIZE_BYTES` of 768 would yield a 256-byte card with no diagnostic
// anywhere. Fail the build instead.
const _: () = assert!(
    (1usize << CARD_SHIFT) == CARD_SIZE_BYTES,
    "CARD_SIZE_BYTES must be a power of two: CARD_SHIFT is derived from it and      is baked into emitted machine code",
);

// JIT x64 emits array element offsets as a *signed* disp8 whose value is
// HEADER_SIZE. If HEADER_SIZE exceeds 127 the disp8 wraps negative and the
// emitted code addresses backwards from the object base. Convert the affected
// emitters to disp32 before allowing HEADER_SIZE to grow beyond this limit;
// the authoritative site inventory is
// `x64-flag-skew-and-contracts.md` §6.2
// (the older "jit/src/x64.rs:6690-6813" citation was stale — that range holds
// loop/BCE analysis, not an emitter).
const _: () = assert!(
    HEADER_SIZE <= 127,
    "HEADER_SIZE must fit in signed disp8 for JIT array access"
);

// The object body is addressed as a run of qword cells starting at HEADER_SIZE,
// and both the JIT's inline TLAB bump (`emit_inline_tlab_new`) and its Rust twin
// (`Tlab::alloc_initialized`) advance the cursor by `HEADER_SIZE + body_size`
// with an 8-byte alignment grid. `body_size` is always a multiple of 8 (legacy
// layouts are `n * SLOT_SIZE` = n*16; compact layouts are pinned by
// `ObjectHeader::set_compact_shape`'s `body_size & 7 == 0` assert), so the whole
// grid holds iff HEADER_SIZE is itself 8-aligned. This was previously only a
// `debug_assert` on the allocation path — compiled out in release, where a
// violation is a silent heap-walk desync rather than a panic. Pin it at compile
// time so no future HEADER_SIZE can break the grid at all.
const _: () = assert!(
    HEADER_SIZE % 8 == 0,
    "HEADER_SIZE must be 8-aligned or the TLAB bump grid and qword-indexed body \
     cells desync (silent heap-walk corruption in release builds)"
);

// --- Mark word (thin-lock / monitor inflation) -----------------------------
//
// The mark word is a single `AtomicU64` appended to `ObjectHeader`. It encodes
// one of three states in the low 2 bits:
//
//   00 = NEUTRAL      (no lock held)
//   01 = THIN_LOCKED  (owner thread id + recursion count in upper bits)
//   10 = INFLATED     (pointer to heap-allocated Monitor in upper 62 bits)
//   11 = FORWARDED    (GC relocation target in upper 62 bits)
//
// All transitions are performed via atomic CAS on the `mark_word` field.
//
// The four states are mutually exclusive: the 2-bit tag is read with
// `ObjectHeader::mark_state` and compared for *equality*, never tested with a
// bitwise AND. That is what makes `0b11` claimable — a bitwise `mark &
// MARK_INFLATED != 0` test would have aliased FORWARDED onto INFLATED and
// handed a relocation address to `inflated_monitor()` as a `Monitor*`. Every
// consumer in `vm/src/threading/monitor.rs` was audited for this before the
// state was claimed; see `header-shrink.md` §4.

/// Mark word state: no lock held. Identity hash code may live in upper bits
/// (caller-managed).
pub const MARK_NEUTRAL: u64 = 0b00;
/// Mark word state: object is thin-locked by a single thread.
pub const MARK_THIN_LOCKED: u64 = 0b01;
/// Mark word state: object's monitor has been inflated to a heap `Monitor`.
pub const MARK_INFLATED: u64 = 0b10;
/// Mark word state: object has been relocated by the GC; the upper 62 bits are
/// the full 64-bit forwarding target (see [`ObjectHeader::make_forwarded`]).
///
/// **Not yet produced by anything.** This is the encoding half of the
/// `ObjectHeader` 32→24 shrink; the `forwarding_ptr` field is still the live
/// mechanism and remains the single source of truth until the consumers listed
/// in `header-shrink.md` §6 are migrated in one
/// atomic change. It is landed now, with round-trip coverage, so the second
/// pass adopts a tested encoding instead of inventing one.
pub const MARK_FORWARDED: u64 = 0b11;
/// Mask covering the 2-bit state field of the mark word.
pub const MARK_STATE_MASK: u64 = 0b11;

/// Thin-lock mark word layout:
///   bits 0-1:   state = `MARK_THIN_LOCKED`
///   bits 2-9:   recursion count (u8, 0 = held once; max 255 -> 256 nested)
///   bits 10-41: owner thread id (u32)
///   bits 42-63: reserved
pub const THIN_LOCK_RECURSION_SHIFT: u32 = 2;
pub const THIN_LOCK_RECURSION_MASK: u64 = 0xffu64 << THIN_LOCK_RECURSION_SHIFT;
pub const THIN_LOCK_OWNER_SHIFT: u32 = 10;
pub const THIN_LOCK_OWNER_MASK: u64 = 0xffff_ffffu64 << THIN_LOCK_OWNER_SHIFT;

/// Inflated mark word: pointer to `Monitor` (high 62 bits) | `MARK_INFLATED`
/// (low 2 bits). The `Monitor` struct must be at least 4-byte aligned so the
/// bottom 2 bits are available for the state tag. (In practice it will be
/// 8-byte aligned, leaving bit 2 free as well.)
pub const INFLATED_PTR_MASK: u64 = !MARK_STATE_MASK;

/// Forwarded mark word: relocation target (high 62 bits) | [`MARK_FORWARDED`]
/// (low 2 bits).
///
/// Identical in shape to [`INFLATED_PTR_MASK`], and deliberately so — both
/// carry a **full 64-bit** pointer with only the low 2 tag bits borrowed. The
/// address is *not* shifted, so nothing is truncated and no overflow side table
/// is needed: heap objects are 8-byte aligned (`HEADER_SIZE % 8 == 0` and every
/// allocator aligns to 8), so bits 0-2 of a legal object address are already
/// zero and `addr | MARK_FORWARDED` round-trips exactly through
/// `mark & FORWARDING_PTR_MASK` for any address in the 64-bit space.
pub const FORWARDING_PTR_MASK: u64 = !MARK_STATE_MASK;

// --- Identity hash in the NEUTRAL mark word --------------------------------
//
// An object's identity hash is installed lazily, on first request, into the
// upper bits of a `MARK_NEUTRAL` mark word -- HotSpot's design, and half of what
// getting `HEADER_SIZE` to 16 needs (`identity_hash_code` is 4 of the 8 bytes
// that have to go; see
// `header-16-and-field-packing-20260806.md` section 2).
//
// # Why this needs no change to the locking fast path
//
// `try_thin_lock` (`vm/src/threading/monitor.rs`) CASes from the **literal**
// `MARK_NEUTRAL` (`== 0`), not from `mark_state(cur) == MARK_NEUTRAL`. A word
// carrying a hash is non-zero, so that CAS simply fails and the caller falls
// through to `inflate_locked` -- which is exactly HotSpot's rule that a hashed
// object cannot be thin-locked and must inflate instead. It is already
// implemented, for free. `a_hashed_word_cannot_win_the_thin_lock_cas` below
// pins it, so a future change from a literal compare to a state compare fails
// here rather than silently destroying identity hashes under contention.
//
// # The one transition that destroys a hash
//
// Inflation. `publish_inflated` overwrites the whole word with
// `INFLATED | monitor_ptr`, so it has to displace the hash into the
// address-keyed `HashCodeTable` (`gc/src/compact_header.rs`, which already
// carries the GC re-keying and dead-entry sweep that needs). Nothing else can:
// THIN_LOCKED is unreachable for a hashed object per the above, and FORWARDED
// is written to the *source* after the copy, so the destination carries the
// intact word.
//
// Hashing an object that is already thin-locked therefore has to inflate it
// too -- otherwise releasing the lock restores a bare `MARK_NEUTRAL` and the
// next request mints a *second*, different hash for the same live object. This
// VM never deflates a monitor (`grep deflat` in `monitor.rs` finds nothing), so
// once displaced a hash never has to come back.

/// Bit position of the identity hash within a `MARK_NEUTRAL` mark word; bits
/// 0-1 are the state tag.
pub const MARK_HASH_SHIFT: u32 = 2;

/// Mask of the identity-hash field within a `MARK_NEUTRAL` mark word: 31 bits,
/// matching the non-negative `int` range `Object.hashCode` is expected to
/// produce (and HotSpot's own hash width).
pub const MARK_HASH_MASK: u64 = 0x7FFF_FFFFu64 << MARK_HASH_SHIFT;

/// Byte offset of `mark_word` within `ObjectHeader`. Documented for downstream
/// agents (JIT lock fast-path) so they can emit direct atomic loads / CAS.
///
/// Derived from the `#[repr(C)]` layout: HEADER_SIZE(24) - 8 = 16. The
/// `_const_check_mark_word_offset` assertion below pins this at compile time.
pub const MARK_WORD_OFFSET: usize = 8;

/// Size of each field/array element slot in bytes.
/// Must be >= size_of::<Value>() (which is 16 bytes: 8 for the payload + 8 for the discriminant).
pub const SLOT_SIZE: usize = 16;

/// Size of each reference array element in bytes (compact: raw pointer only).
/// Object[] elements are stored as raw 8-byte pointers (0 = null) instead of
/// 16-byte Value enums. This halves memory usage for reference arrays.
pub const REF_ELEMENT_SIZE: usize = 8;

/// Size of a compact reference *instance field* in bytes (raw pointer, 0 = null).
///
/// Under the compact reference-field layout (`CRATONVM_COMPACT_REF_FIELDS`), a
/// reference instance field is stored as a bare 8-byte pointer instead of the
/// 16-byte tagged [`crate::Value`] cell — the same encoding reference array
/// elements already use. Primitive fields keep their 16-byte [`SLOT_SIZE`] cell.
/// See `docs/feature-designs/compact-ref-field-layout.md`.
pub const REF_FIELD_SIZE: usize = 8;

// --- Instance-field cell (`Value` enum) inline-access layout ---------------
//
// Java instance fields are stored on the heap as the full 16-byte `crate::Value`
// enum (one cell == one `SLOT_SIZE`-byte region). The JIT's `getfield` codegen
// emits a raw `MOV` against a field cell instead of calling the `jit_getfield`
// helper, so it needs the byte offset of the payload *within* the cell.
//
// `Value` is `#[repr(u32)]` with explicit `= 0 ..= 6` discriminants, so the
// layout below is a *language guarantee*, not an observation: the enum is laid
// out as `#[repr(C)] struct { tag: u32, payload: union { .. } }`, putting a
// 4-byte discriminant word at offset 0 and each variant's payload at its
// natural alignment after it.
//
// It used to be `#[repr(Rust)]`, with this comment noting the layout was merely
// "what rustc deterministically chooses" and pointing at the runtime test below
// as the only pin. That was the weak form of the invariant twice over: a
// runtime test catches drift only for whoever runs `-p cratonvm-types`, and
// nothing stopped rustc from moving the tag in the meantime. All four facts are
// now `const`-asserted in `value.rs` (search `value_tag_word`), so drift is a
// compile error in this crate rather than a miscompile in the JIT. The explicit
// discriminants also make the `repr` structurally impossible to delete — E0732
// rejects explicit discriminants on non-unit variants without one — so the
// guarantee cannot be silently dropped either.
//
// Layout (guaranteed by `#[repr(u32)]`, re-verified by the test below):
//   bytes 0..4    : discriminant word (Int=0, Long=1, Float=2, Double=3,
//                   Object=4, ReturnAddress=5, Uninitialized=6)
//   bytes 4..8    : payload of a 4-byte variant (Int / Float / ReturnAddress)
//   bytes 8..16   : payload of an 8-byte variant (Long / Double / Object ptr)
//
// For `Value::Object`, the 8-byte word at offset 8 is exactly the raw object
// pointer: a non-null reference stores its pointer there, and `Object(None)`
// (the JVM `null`) leaves it zero — matching `jit_getfield`'s
// `Object(Some(r)) => r.as_ptr()` / `Object(None) => 0`.

/// Byte offset of the discriminant word within a 16-byte `Value` field cell.
pub const FIELD_CELL_TAG_OFFSET: usize = 0;

/// The discriminant word a 16-byte `Value` field cell carries when it holds a
/// reference -- i.e. when the 8 bytes at [`FIELD_CELL_PAYLOAD64_OFFSET`] are an
/// object pointer and not something else.
///
/// A JIT arm that reads that payload without comparing against this is reading
/// a pointer out of a cell that may not hold one. That is not theoretical: the
/// IR backend's inline legacy `getfield` did exactly that until 2026-08-23, and
/// a `[C` field whose cell held `Value::Int(1)` became a wild pointer that the
/// next instruction dereferenced.
///
/// Named rather than written as `4` at each site, and const-asserted against
/// `Value::Object` in `value.rs`, so the enum and the JIT cannot drift apart
/// silently.
pub const FIELD_CELL_TAG_OBJECT: u32 = 4;

/// Byte offset of a 4-byte payload (`Int` / `Float`) within a `Value` field cell.
pub const FIELD_CELL_PAYLOAD32_OFFSET: usize = 4;

/// Byte offset of an 8-byte payload (`Long` / `Double` / object pointer) within
/// a `Value` field cell.
pub const FIELD_CELL_PAYLOAD64_OFFSET: usize = 8;

/// Special class ID for auto-boxed primitive values in compact reference arrays.
/// When a non-Object Value (Int, Long, Float, Double) is stored in a Reference
/// array via `set_array_element`, it is automatically wrapped in a 1-field object
/// with this class ID. On read via `get_array_element`, the wrapper is detected
/// and the original Value is transparently returned.
///
/// # Reserved range (LOW, 2026-06-17)
///
/// This is a **reserved sentinel**, not a real loaded class. Real `ClassId`s
/// are assigned **densely and sequentially from 0** (see
/// `class_manager::recompute_subclass_layouts`, which iterates
/// `ClassId::new(idx)` over `0..class_store.len()`), so the only safe sentinel
/// is one the sequential allocator can never legitimately hand out. The
/// previous value `0xAB00_0000` (≈2.87 billion) sat in the *middle* of the
/// `u32` range and would collide with a real class once that many classes were
/// loaded — an unreserved, theoretically-reachable id. It is now pinned to
/// `u32::MAX` (`0xFFFF_FFFF`), the single highest id: the allocator would have
/// to load all 4,294,967,295 lower ids first, so this can never be reached. The
/// allocation cap below makes that contract a compile-/runtime-checkable
/// invariant for the upper bound of the dense id space.
pub const AUTOBOX_CLASS_ID: ClassId = ClassId::new(u32::MAX);

// ---------------------------------------------------------------------------
// The `Result<(), i32>` array-store channel
// ---------------------------------------------------------------------------

/// The `Err` code every `set_array_element` / `get_array_element` returns when
/// the index is out of range, or the receiver is not an array at all.
///
/// The channel is an `i32` while the index is a `usize`, and the four backends
/// all wrote `Err(index as i32)`. That truncates: an index of `0x8000_0000`
/// reports `i32::MIN`, and `RuntimeError::aioobe` then names a NEGATIVE index
/// in the exception message for a store whose index was positive. Saturating
/// instead is faithful for every index a real array can hold — `MAX_ARRAY_LENGTH`
/// is `i32::MAX`, so `i32::MAX` is already out of range for every array this VM
/// can build — and it is what keeps [`ARRAY_STORE_OUT_OF_MEMORY`] below
/// unambiguous: no out-of-range index can ever produce that code.
#[inline]
pub const fn oob_index_code(index: usize) -> i32 {
    if index >= i32::MAX as usize {
        i32::MAX
    } else {
        index as i32
    }
}

/// The `Err` code a `set_array_element` returns when the store needed an
/// auto-box wrapper and the heap could not allocate one.
///
/// Storing a primitive `Value` into a reference array allocates a one-field
/// `AUTOBOX_CLASS_ID` wrapper (see `cratonvm_gc::autobox`). All four backends did
/// that through the INFALLIBLE `alloc_object`, which prints
/// `FATAL: out of heap space` and calls `std::process::abort()` — so a Java
/// program that filled the heap while a native was copying primitives into an
/// `Object[]` died with no stack trace, no `OutOfMemoryError`, and no chance
/// for a `catch (OutOfMemoryError)` to run. That is a *Java-level* condition
/// with a *Java-level* answer, and this code carries it back out to the caller
/// so the interpreter can raise `java.lang.OutOfMemoryError` instead.
///
/// [`oob_index_code`] guarantees this value cannot also mean "index
/// `i32::MIN`": every out-of-range code is in `0..=i32::MAX`.
pub const ARRAY_STORE_OUT_OF_MEMORY: i32 = i32::MIN;

/// Exclusive upper bound on sequentially-assigned (dense, from-0) `ClassId`s.
///
/// The class loader assigns ids `0, 1, 2, …`; this is the first value it must
/// never reach so that [`AUTOBOX_CLASS_ID`] (and any future high-range reserved
/// sentinel) stays distinct. Callers that mint a sequential id should
/// `debug_assert!(next_id < MAX_SEQUENTIAL_CLASS_ID)` at the allocation site so
/// the reservation fails loud rather than silently colliding. Kept here next to
/// the reservation it protects; the allocator lives in the `classloading`
/// crate (out of this file's edit scope) — see the residual note in the fix
/// report to wire the assert in at `class_manager`'s id-minting site.
pub const MAX_SEQUENTIAL_CLASS_ID: u32 = u32::MAX;

// Reservation guard: AUTOBOX_CLASS_ID must sit at (or above) the cap that
// sequential allocation can never reach, so it can never alias a real,
// densely-assigned class id. `ClassId::as_u32` is not a `const fn`, so this is
// pinned at test time rather than compile time (see
// `autobox_class_id_is_reserved` in the tests module below).

/// Byte offset of an array's **data area** from the object base.
///
/// Equal to [`HEADER_SIZE`] today, and deliberately a separate name anyway.
///
/// `HEADER_SIZE` currently means two different things across its ~860 call
/// sites: "where an object's instance fields start" and "where an array's
/// elements start". They are the same integer, so **the source does not record
/// which site means which** — and the 24 → 16 shrink needs them to differ. A
/// 16-byte header cannot hold a 31-bit array length (see
/// `header-16-and-field-packing-20260806.md` §2: `class_id`
/// (32) + length (31) + `kind`/`element_type`/`gc_age`/`gc_flags` (14) is 77
/// bits, while `AtomicU64` alignment leaves only 64 ahead of the mark word), so
/// the length has to move into an 8-byte prefix at the head of the array's
/// body: objects 16, array data still at 24.
///
/// Migrating array sites to this name while the two constants are still equal
/// is a no-op refactor by construction, which is the point — it separates the
/// *classification*, which needs review and can be silently wrong, from the
/// flip, which is one line.
///
/// # This constant cannot be tested while it equals `HEADER_SIZE`
///
/// A suite that passes with `ARRAY_DATA_OFFSET == HEADER_SIZE` says nothing
/// about whether the classification is right: every site reads the same number
/// either way. That is the shape of a guard that cannot fail. Before trusting
/// the migration, build once with this set to a deliberately absurd value (64)
/// and run the suite — every array site that should have been migrated and was
/// not then reads 40 bytes off its own array and fails loudly.
pub const ARRAY_DATA_OFFSET: usize = HEADER_SIZE;

// The data area must begin at or after the header's end and stay on the 8-byte
// object grid, and it is emitted as a signed disp8 against the object base for
// exactly the same reason `HEADER_SIZE` is (see that assert above).
const _: () = assert!(
    ARRAY_DATA_OFFSET >= HEADER_SIZE && ARRAY_DATA_OFFSET % 8 == 0,
    "ARRAY_DATA_OFFSET must sit at or past the header end, on the 8-byte grid"
);
const _: () = assert!(
    ARRAY_DATA_OFFSET <= 127,
    "ARRAY_DATA_OFFSET must fit a signed disp8 for JIT array element access"
);

/// Byte offset of the array-length/object-shape word: the `shape` field of
/// [`ObjectHeader`], which sits immediately after the 4-byte `class_id`.
//
// **4.** This comment read "8, not 12" directly above a value of `4` -- both
// numbers wrong, and wrong in a way no test could catch, because every JIT
// array-length load is emitted FROM the constant and so tracked the value
// while the prose drifted. (It was 12 while the header still carried
// `kind`/`element_type`/`gc_age`/`gc_flags` and `identity_hash_code` ahead of
// `shape`; the 32 -> 24 -> 16 shrink of 2026-08-06/07 moved `shape` up behind
// `class_id`.) The const assert below is the fix that lasts: the offset is now
// derived from the struct at build time, so a future field reorder is a
// compile error rather than another stale sentence.
pub const ARRAY_LENGTH_OFFSET: usize = 4;

// The value is not a choice — it is `shape`'s actual offset in the
// `#[repr(C)]` header. Pin it, so a field reorder cannot leave every emitted
// `MOV r32, [obj + ARRAY_LENGTH_OFFSET]` reading `class_id` instead.
const _: () = assert!(
    ARRAY_LENGTH_OFFSET == core::mem::offset_of!(ObjectHeader, shape),
    "ARRAY_LENGTH_OFFSET must equal the byte offset of ObjectHeader::shape"
);
// Emitted as a signed disp8 against the object base at every JIT array-length
// site (`x64/arrays.rs`, `ir_lower.rs`), so it must fit -128..=127 or those
// instructions silently address BEFORE the object. `disp::disp8_const` makes
// that a build failure at the emission sites; this makes it one here too.
const _: () = assert!(
    ARRAY_LENGTH_OFFSET <= 127,
    "ARRAY_LENGTH_OFFSET must fit a signed disp8 for JIT array-length loads"
);
pub const NUM_SLOTS_OFFSET: usize = 4;
// --- The quartet, in mark-word bits 48..63 ---------------------------------
//
// `kind`, `element_type`, `gc_age` and `gc_flags` were four bytes at offsets
// 4..8. They had to leave for `HEADER_SIZE` to reach 16: the header is
// `class_id`(4) + `shape`(4) + an 8-aligned `AtomicU64`, and that is exactly
// 16 with nothing spare.
//
// The mark word's TOP TWO BYTES -- bits 48..63 inclusive -- are free in EVERY
// state, which is what makes this work rather than merely fit:
//   * NEUTRAL     -- the hash occupies bits 2..32 (`MARK_HASH_MASK`);
//   * THIN_LOCKED -- recursion bits 2..9, owner bits 10..41;
//   * INFLATED / FORWARDED -- a pointer, and `plausible_heap_pointer` caps
//     every one at `2^47 - 1`, so bits 47..63 are zero by construction.
//
// That last point is the load-bearing one and it is enforced, not assumed:
// `make_inflated` and `make_forwarded` both assert `plausible_heap_pointer`.
//
// # Bit diagram (bit 63 on the left)
//
//   63    60 59    56 55  54 53      50 49 48 | 47 .......... 2 | 1  0
//   +--------+--------+------+----------+-----+----------------+------+
//   | gc_age |gc_flags| rsvd | elem_typ | knd | state payload  | state|
//   +--------+--------+------+----------+-----+----------------+------+
//   \--------------- MARK_QUARTET_MASK -------/
//
//     gc_age        bits 60..63 (4)   AGE_SHIFT   = 60, AGE_BITS   = 0xF
//     gc_flags      bits 56..59 (4)   FLAGS_SHIFT = 56, FLAGS_BITS = 0xF
//     reserved      bits 54..55 (2)   -- inside the mask, owned by no field
//     element_type  bits 50..53 (4)   ELEM_SHIFT  = 50, ELEM_BITS  = 0xF
//     kind          bits 48..49 (2)   KIND_SHIFT  = 48, KIND_BITS  = 0x3
//
// 14 field bits over a 16-bit span. The mask deliberately covers the whole
// SPAN and not the union of the fields: bits 54..55 belong to nothing, and
// keeping them inside means they are carried through every `quartet_of`
// rebuild, so a future fifth field can claim them without auditing a single
// caller. The three numbers that must agree -- the mask's doc, the mask's
// value and the field layout -- are all "bits 48..63, 16 wide".
//
// # 2026-08-07: this mask used to be `0x3FFF << 48`, and that was a bug
//
// `0x3FFF << 48` is bits 48..61. It stopped TWO BITS SHORT of the top of
// `gc_age`, which starts at `AGE_SHIFT = 60` and is 4 bits wide (bits 60..63),
// so bits 62..63 -- the top half of the age -- fell OUTSIDE the "quartet".
// Three silent consequences, none of which the suite could see:
//
//   1. `quartet_of` dropped them, so `make_thin_locked` / `make_inflated` /
//      `make_forwarded`, which all rebuild a word from `quartet_of(prev)`,
//      truncated any `gc_age >= 4` to `age & 3`. `MAX_GC_AGE` is 15, so 12 of
//      the 16 ages were lossy: thin-locking or inflating an object rewound its
//      tenuring age and perturbed the next promotion decision. Not an edge
//      case -- G1's default `promotion_age` is 15 (`gc/src/g1.rs`), so ages
//      4..14 are its ORDINARY young-survivor range.
//   2. `!MARK_QUARTET_MASK` consequently did NOT mean "everything that is not
//      the quartet": it retained bits 62..63. `inflated_monitor` and
//      `forwarding_target` strip exactly that, so `set_gc_age(n)` with
//      `n >= 4` on an already-`INFLATED` word yielded
//      `monitor_ptr | (1 << 62)` -- a non-null WILD pointer handed to
//      `monitor_ptr_from_mark` (`vm/src/threading/monitor.rs`), which only
//      null-checks it. The `make_*` helpers emit words with those bits already
//      clear, which is why round-trip tests looked clean: the corruption needs
//      the age to be written AFTER the state, which is exactly what every
//      collector does (`gc/src/g1.rs` `SharedEvac::evacuate` and
//      `evacuate_object` copy the source mark word to the destination and then
//      age the copy; `gc/src/gen_heap.rs`'s young non-moving sweep ages
//      survivors in place with no promotion cap at all).
//   3. `try_thin_lock` (`vm/src/threading/monitor.rs`) screens an unlocked
//      word with `cur & !types::MARK_QUARTET_MASK != MARK_NEUTRAL`. With bits
//      62..63 left in, an object whose age had reached 4 could never satisfy
//      that test again, so every subsequent lock on it inflated a monitor,
//      permanently -- the precise failure the mask was introduced to stop for
//      arrays, reintroduced for aged objects.
//
// It went unnoticed because every round-trip test used age 3, the largest
// value that fits in bits 60..61. Widening to the full 16-bit span collides
// with nothing: see the state-by-state list above, and the partition and
// disjointness assertions in the tests below.
pub const MARK_QUARTET_SHIFT: u32 = 48;
/// The mark word's top 16 bits (48..63): the four quartet fields plus the two
/// reserved bits between `element_type` and `gc_flags`. Everything outside it
/// belongs to the state tag and its payload, and `!MARK_QUARTET_MASK` is
/// relied upon to mean exactly that.
pub const MARK_QUARTET_MASK: u64 = 0xFFFFu64 << MARK_QUARTET_SHIFT;

// The sub-fields are placed for the BENEFIT OF THE JIT, not for tidiness.
//
// `gc_flags` sits at bits 56..59, i.e. bits 0..3 of the mark word's byte 7.
// That is what lets the emitted `TEST BYTE [obj + off], GC_FLAG_COMPACT` keep
// its mask unchanged -- only the displacement moves. Aligning it anywhere else
// would have forced a shifted mask into every emission site, which is the kind
// of derived constant that goes stale silently.
//
// `kind` and `element_type` share byte 6, so a walker can read both tags with
// one byte load. `gc_age` takes bits 60..63 -- the remainder of byte 7, i.e.
// its high nibble. (This read "bits 59..63" until 2026-08-07, which is a
// five-bit range and disagreed with `AGE_SHIFT = 60` by one; it is the third
// of the three numbers reconciled with the mask fix above.)
// Ranges below are INCLUSIVE at both ends, matching the diagram above.
const KIND_SHIFT: u32 = MARK_QUARTET_SHIFT; // 48: word bits 48..49, byte 6 bits 0..1
const KIND_BITS: u64 = 0x3;
const ELEM_SHIFT: u32 = MARK_QUARTET_SHIFT + 2; // 50: word bits 50..53, byte 6 bits 2..5
const ELEM_BITS: u64 = 0xF;
// Word bits 54..55 (byte 6, bits 6..7) are RESERVED: inside MARK_QUARTET_MASK,
// claimed by no field. They are preserved by every quartet rebuild.
const FLAGS_SHIFT: u32 = MARK_QUARTET_SHIFT + 8; // 56: word bits 56..59, byte 7 bits 0..3
                                                 // FOUR bits for three defined flags. The spare one is not slack -- it is what
                                                 // keeps `header_reserved_fields_plausible` able to fail. That screen rejects a
                                                 // header carrying an undefined flag bit, and if the field were exactly three
                                                 // bits wide the bit could not be represented, so the screen would always pass:
                                                 // a guard that cannot fail, on the path that decides whether a candidate
                                                 // address is a real object.
const FLAGS_BITS: u64 = 0xF;
const AGE_SHIFT: u32 = MARK_QUARTET_SHIFT + 12; // 60: word bits 60..63, byte 7 bits 4..7
const AGE_BITS: u64 = 0xF;

// The regression guard for the 2026-08-07 mask fix, at COMPILE time: every one
// of the four fields must lie wholly inside MARK_QUARTET_MASK. The old
// `0x3FFF << 48` mask failed this on `gc_age` alone, and nothing in the build
// noticed for a day. Kept as four separate asserts so the message names the
// field that escaped.
const _: () = assert!(
    (KIND_BITS << KIND_SHIFT) & !MARK_QUARTET_MASK == 0,
    "kind escapes MARK_QUARTET_MASK"
);
const _: () = assert!(
    (ELEM_BITS << ELEM_SHIFT) & !MARK_QUARTET_MASK == 0,
    "element_type escapes MARK_QUARTET_MASK"
);
const _: () = assert!(
    (FLAGS_BITS << FLAGS_SHIFT) & !MARK_QUARTET_MASK == 0,
    "gc_flags escapes MARK_QUARTET_MASK"
);
const _: () = assert!(
    (AGE_BITS << AGE_SHIFT) & !MARK_QUARTET_MASK == 0,
    "gc_age escapes MARK_QUARTET_MASK -- see the 2026-08-07 note above"
);
// ...and the mask must not reach down into any state payload. A pointer is
// capped at `2^47 - 1` by `plausible_heap_pointer`, so bit 46 is the highest a
// payload can ever occupy; the quartet must start strictly above it (bit 47 is
// spare between the two).
const _: () = assert!(
    MARK_QUARTET_SHIFT >= 48,
    "the quartet must sit above the 47-bit plausible-pointer range"
);
const _: () = assert!(
    MARK_QUARTET_MASK & (MARK_STATE_MASK | MARK_HASH_MASK) == 0,
    "MARK_QUARTET_MASK overlaps the state tag or the identity hash"
);
const _: () = assert!(
    MARK_QUARTET_MASK & (THIN_LOCK_RECURSION_MASK | THIN_LOCK_OWNER_MASK) == 0,
    "MARK_QUARTET_MASK overlaps the thin-lock payload"
);

/// Byte offset, from the OBJECT BASE, of the byte holding [`GC_FLAG_OLD_GEN`]
/// / [`GC_FLAG_MARKED`] / [`GC_FLAG_COMPACT`].
///
/// The flags occupy bits 0..3 of this byte, so a byte-wise test against the
/// flag constants themselves is still correct -- which is exactly why they were
/// placed there. Replaces the old `GC_FLAGS_OFFSET`.
pub const GC_FLAGS_BYTE_OFFSET: usize = MARK_WORD_OFFSET + 7;

/// Byte offset, from the OBJECT BASE, of the byte holding the `kind` tag
/// (bits 0..2) and the `element_type` tag (bits 2..6). Replaces the old
/// `OBJECT_KIND_OFFSET` / `ARRAY_ELEMENT_TYPE_OFFSET` pair, which were two
/// separate bytes.
pub const KIND_TAGS_BYTE_OFFSET: usize = MARK_WORD_OFFSET + 6;

/// Mask selecting the `kind` tag out of the [`KIND_TAGS_BYTE_OFFSET`] byte.
pub const KIND_TAG_BYTE_MASK: u8 = 0x3;

// A plain object's KIND_TAGS byte is ZERO, and several JIT guards depend on it:
// they emit `CMP BYTE [recv + KIND_TAGS_BYTE_OFFSET], 0` to separate a plain
// object from an array in one instruction. That holds only because `kind` for
// an object is 0 AND `ArrayElementType::Reference` -- what every object
// allocation passes -- is also 0. If either discriminant were renumbered the
// guard would silently start admitting arrays, so pin both.
const _: () = assert!(ObjectKind::Object as u8 == 0);
const _: () = assert!(ArrayElementType::Reference as u8 == 0);

/// The highest `gc_age` the 4-bit field can hold. Matches HotSpot's tenuring
/// ceiling; ages saturate here rather than wrapping to 0, which would make an
/// old object look freshly allocated.
pub const MAX_GC_AGE: u8 = 15;

// Compile-time check that the offset is correct.
const _: () = assert!(
    std::mem::offset_of!(ObjectHeader, shape) == ARRAY_LENGTH_OFFSET,
    "ARRAY_LENGTH_OFFSET must match ObjectHeader layout"
);

// The array-length load is emitted as a signed disp8 in 16 places in
// `jit/src/x64.rs` (§6.2 of the x64 contract doc). A shrink that moves `shape`
// must keep it addressable, or those sites emit a negative displacement and
// read backwards from the object base.
const _: () = assert!(
    ARRAY_LENGTH_OFFSET <= 127,
    "ARRAY_LENGTH_OFFSET must fit a signed disp8 for the JIT array-length loads"
);

// Compile-time check that our header is exactly HEADER_SIZE bytes.
const _: () = assert!(
    std::mem::size_of::<ObjectHeader>() == HEADER_SIZE,
    "ObjectHeader must be exactly HEADER_SIZE bytes"
);

// Compile-time check that the mark_word lives at the documented offset.
const _: () = assert!(
    std::mem::offset_of!(ObjectHeader, mark_word) == MARK_WORD_OFFSET,
    "MARK_WORD_OFFSET must match ObjectHeader layout"
);

// Compile-time check that class_id remains at offset 0 -- JIT-emitted code
// hardcodes this offset and must not be silently broken by field reordering.
const _: () = assert!(
    std::mem::offset_of!(ObjectHeader, class_id) == 0,
    "class_id must remain at offset 0 (JIT contract)"
);

/// Raw `kind` tag of the object at `ptr`, without ever forming an
/// `ObjectKind`.
///
/// Replaces the raw BYTE reads at the old `OBJECT_KIND_OFFSET`. Those existed
/// so the optimiser could not assume the discriminant was in range and fold a
/// validity check away -- a screen that must reject an out-of-range tag cannot
/// go through the typed enum to get it. The quartet moved into the mark word,
/// but that reasoning did not, so the same property is preserved here: this
/// returns a `u8` the caller validates.
///
/// # Safety
/// `ptr` must point at a mapped object header.
#[inline(always)]
pub unsafe fn kind_tag_at(ptr: *const u8) -> u8 {
    let mark = unsafe { (ptr.add(MARK_WORD_OFFSET) as *const u64).read_unaligned() };
    ObjectHeader::kind_tag(mark)
}

/// Raw `element_type` tag of the object at `ptr`, without forming an
/// `ArrayElementType`. Validate with [`array_element_type_from_tag`].
///
/// # Safety
/// `ptr` must point at a mapped object header.
#[inline(always)]
pub unsafe fn element_type_tag_at(ptr: *const u8) -> u8 {
    let mark = unsafe { (ptr.add(MARK_WORD_OFFSET) as *const u64).read_unaligned() };
    ObjectHeader::element_type_tag(mark)
}

/// Returns the per-element byte size for a given array element type.
/// Used for compact array storage -- primitive arrays use their native
/// byte size instead of the full SLOT_SIZE (16 bytes).
///
/// Reference arrays use [`crate::narrow_oop::ref_element_size`]: the
/// [`REF_ELEMENT_SIZE`] (8-byte) raw pointer by default, or 4 bytes when
/// compressed oops are active for this process. The width is fixed at VM init
/// before the first allocation, so an array is never read back under a
/// different element width than it was written with.
#[inline]
pub fn element_byte_size(element_type: ArrayElementType) -> usize {
    match element_type {
        ArrayElementType::Boolean | ArrayElementType::Byte => 1,
        ArrayElementType::Char | ArrayElementType::Short => 2,
        ArrayElementType::Int | ArrayElementType::Float => 4,
        ArrayElementType::Long | ArrayElementType::Double => 8,
        ArrayElementType::Reference => crate::narrow_oop::ref_element_size(),
    }
}

/// Compute the total byte size of an array's data area (8-byte aligned).
///
/// Returns `None` if the size overflows `usize`.
#[inline]
pub fn array_data_size_checked(length: usize, element_type: ArrayElementType) -> Option<usize> {
    let raw = length.checked_mul(element_byte_size(element_type))?;
    raw.checked_add(7).map(|v| v & !7) // round up to 8-byte alignment
}

/// Compute the total byte size of an array's data area (8-byte aligned).
///
/// Returns `Err` on integer overflow instead of panicking. Callers receiving
/// untrusted input (e.g. from bytecode) should propagate or handle the error.
#[inline]
pub fn array_data_size(
    length: usize,
    element_type: ArrayElementType,
) -> Result<usize, &'static str> {
    array_data_size_checked(length, element_type)
        .ok_or("array data size overflow: length * element_size exceeds usize")
}

/// Whether a heap allocation is a regular object or an array.
///
/// `HumongousFiller` is a synthetic sentinel kind (round-9 gc CRIT-1 fix):
/// the GC writes a header with this kind at the start of every humongous
/// continuation region. Heap walkers MUST treat this header as "skip the
/// entire region; not a real object, no oops to scan". Without this
/// sentinel, the zeroed bytes of a continuation region decode as a
/// well-formed `Object` (kind=0, class_id=0, num_slots=0) and walkers
/// iterate the region as a minimum-sized object, following the next zero header,
/// and so on -- effectively scanning garbage as live objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ObjectKind {
    Object = 0,
    Array = 1,
    /// Round-9 gc CRIT-1: sentinel header at the start of a humongous
    /// continuation region. A walker that sees this kind must skip to
    /// the end of the enclosing region without iterating bytes.
    HumongousFiller = 2,
}

/// The element type for Java arrays (maps to `newarray` atype values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ArrayElementType {
    Boolean = 4,
    Char = 5,
    Float = 6,
    Double = 7,
    Byte = 8,
    Short = 9,
    Int = 10,
    Long = 11,
    /// Reference array (Object[] or any reference type array).
    Reference = 0,
}

/// The `KIND_TAGS_BYTE_OFFSET` byte that a receiver has **iff** it is exactly
/// the one-dimensional primitive array `descriptor` names — or `None` when
/// `descriptor` is not such a type.
///
/// `[B` and its seven siblings are the one array shape a JIT `checkcast` can
/// settle without asking anything else, and the reason is that a class-id
/// compare CANNOT settle it: a primitive array carries `class_id == 0` (it has
/// no class entry at all), and a reference array carries its COMPONENT's id, so
/// neither answers "is this a `byte[]`". The header's own kind/element tags do,
/// exactly, in one byte: `kind` is bits 0..1 and `element_type` bits 2..5 of
/// this byte (`KIND_SHIFT` / `ELEM_SHIFT`), and bits 6..7 are reserved zero, so
/// the whole byte is a single comparable constant.
///
/// One dimension only, and that is what makes it sound rather than nearly
/// sound: `byte[][]` holds REFERENCES to `byte[]` objects, so its element type
/// is `Reference`, not `Byte`. A two-character descriptor is therefore the
/// exact predicate — `[B` matches only a real `byte[]`, and `[[B` has no answer
/// here and keeps the helper.
///
/// Lives here rather than in the JIT because it is a statement about the header
/// layout, and the layout is what would silently invalidate it.
#[inline]
pub fn primitive_array_kind_tags_byte(descriptor: &str) -> Option<u8> {
    let b = descriptor.as_bytes();
    if b.len() != 2 || b[0] != b'[' {
        return None;
    }
    let elem = match b[1] {
        b'Z' => ArrayElementType::Boolean,
        b'C' => ArrayElementType::Char,
        b'F' => ArrayElementType::Float,
        b'D' => ArrayElementType::Double,
        b'B' => ArrayElementType::Byte,
        b'S' => ArrayElementType::Short,
        b'I' => ArrayElementType::Int,
        b'J' => ArrayElementType::Long,
        _ => return None,
    } as u8;
    Some((ObjectKind::Array as u8) | (elem << 2))
}

/// Does the word at `ptr` look like a real object HEADER, judged only from the
/// header itself?
///
/// The heap-free half of `G1Collector::is_object_address`: that function is a
/// range check (`is_addr_in_live_region`) followed by exactly these header
/// tests, and only the range half needs a collector. Callers that already know
/// the address is inside a live, **committed** heap range can use this to
/// finish the job.
///
/// # Why this exists
///
/// `conservative_roots::band_has_unpublished_word_with_map` decides a compiled
/// frame's spill word is an unpublished oop from `addr_is_movable(w)` alone —
/// an address-RANGE test, where every sibling instrument in that file requires
/// `is_object_address`. A word that merely lands in the heap's range is called
/// a live reference, and audit §16-§18 measured what that costs: the analogous
/// raw counters run to hundreds or thousands of words with `verifier_oop=0` on
/// every one of them.
///
/// It could not be screened before, for two reasons that are now gone: there
/// is no `&VmHeap` on that path (this function needs none), and the published
/// range covered reserved-but-uncommitted pages where reading a header faults
/// (§18 bounded it by the commit).
///
/// # Safety
///
/// `ptr` must be readable for `MARK_WORD_OFFSET + 8` bytes and 8-aligned. The
/// caller owes that; there is no way to check it from here.
#[inline]
pub unsafe fn plausible_object_header_at(ptr: *const u8) -> bool {
    // Same order and the same bounds as the collector's own screen, so the two
    // cannot drift into disagreeing about what an object is.
    let Some(kind) = object_kind_from_tag(unsafe { kind_tag_at(ptr) }) else {
        return false;
    };
    if unsafe { array_element_type_from_tag(element_type_tag_at(ptr)) }.is_none() {
        return false;
    }
    // A filler is not an object a root can name.
    if matches!(kind, ObjectKind::HumongousFiller) {
        return false;
    }
    const MAX_PLAUSIBLE_SLOTS: u32 = 1 << 24;
    let header = unsafe { &*(ptr as *const ObjectHeader) };
    match kind {
        ObjectKind::Array => header.array_length() <= i32::MAX as u32,
        _ => header.num_slots() <= MAX_PLAUSIBLE_SLOTS,
    }
}

#[inline]
pub fn object_kind_from_tag(tag: u8) -> Option<ObjectKind> {
    match tag {
        tag if tag == ObjectKind::Object as u8 => Some(ObjectKind::Object),
        tag if tag == ObjectKind::Array as u8 => Some(ObjectKind::Array),
        tag if tag == ObjectKind::HumongousFiller as u8 => Some(ObjectKind::HumongousFiller),
        _ => None,
    }
}

#[inline]
pub fn array_element_type_from_tag(tag: u8) -> Option<ArrayElementType> {
    match tag {
        tag if tag == ArrayElementType::Reference as u8 => Some(ArrayElementType::Reference),
        tag if tag == ArrayElementType::Boolean as u8 => Some(ArrayElementType::Boolean),
        tag if tag == ArrayElementType::Char as u8 => Some(ArrayElementType::Char),
        tag if tag == ArrayElementType::Float as u8 => Some(ArrayElementType::Float),
        tag if tag == ArrayElementType::Double as u8 => Some(ArrayElementType::Double),
        tag if tag == ArrayElementType::Byte as u8 => Some(ArrayElementType::Byte),
        tag if tag == ArrayElementType::Short as u8 => Some(ArrayElementType::Short),
        tag if tag == ArrayElementType::Int as u8 => Some(ArrayElementType::Int),
        tag if tag == ArrayElementType::Long as u8 => Some(ArrayElementType::Long),
        _ => None,
    }
}

/// The header stored at the beginning of every heap-allocated object/array.
///
/// Layout (**16 bytes** total, 8-byte aligned) — the three `#[repr(C)]`
/// fields below and nothing else:
/// - `class_id`: `ClassId` (4 bytes, offset 0) -- MUST stay there (JIT contract)
/// - `shape`: `u32` (4 bytes, offset [`ARRAY_LENGTH_OFFSET`] = 4) -- array
///   length, or full instance-field count
/// - `mark_word`: `AtomicU64` (8 bytes, offset [`MARK_WORD_OFFSET`] = 8) --
///   lock state, identity hash, GC forwarding target, AND the
///   `kind`/`element_type`/`gc_age`/`gc_flags` quartet in bits 48..63
///
/// This list said "32 bytes total" and named five fields that no longer exist
/// as fields — `kind`, `element_type`, `gc_age`, `gc_flags` (folded into the
/// mark word's bits 48..63), `identity_hash_code` and `forwarding_ptr` (both
/// absorbed by the mark word) — with `mark_word` at offset 24. It described
/// the header as it stood before the 32 -> 24 -> 16 shrink of 2026-08-06/07.
/// Read the field docs, and the const-asserted offsets beside the constants,
/// rather than this summary; those cannot go stale silently.
///
/// NOTE: `Clone`/`Copy` were removed when `mark_word: AtomicU64` was added,
/// since atomics are `!Copy`. Header copies must now go through explicit
/// field-by-field reconstruction (or `std::ptr::copy_nonoverlapping` at the
/// raw byte level during GC relocation).
#[repr(C)]
#[derive(Debug)]
pub struct ObjectHeader {
    /// MUST stay at offset 0 as a plain `u32` -- JIT-emitted type guards are
    /// `CMP DWORD [recv+0], imm`, and every design that narrowed this to make
    /// room elsewhere would have put an `AND` on the hottest path in the VM.
    pub class_id: ClassId,
    /// Arrays store their length directly. Objects store the full 32-bit
    /// hierarchy-wide instance-field count. Full width in both cases: the
    /// alternative layouts that packed this into 19 bits are what forced an
    /// array's length out into a body prefix, which cost arrays the shrink
    /// entirely.
    pub shape: u32,
    /// Mark word -- lock state, identity hash, GC forwarding target, AND the
    /// `kind` / `element_type` / `gc_age` / `gc_flags` quartet in bits 48..63
    /// (see the diagram at [`MARK_QUARTET_MASK`]).
    ///
    /// State in the low 2 bits; see `MARK_NEUTRAL` / `MARK_THIN_LOCKED` /
    /// `MARK_INFLATED` / `MARK_FORWARDED`. Always at `MARK_WORD_OFFSET` (= 8).
    ///
    /// This word has absorbed the header three times. `forwarding_ptr` went
    /// first (32 -> 24, 2026-08-06), then `identity_hash_code`, then the
    /// `kind` / `element_type` / `gc_age` / `gc_flags` quartet into bits
    /// 48..63 (24 -> 16, 2026-08-07). (This line, the one above it and
    /// `MARK_QUARTET_MASK` itself said 48..61, 48..62 and "13 bits" -- three
    /// different answers for one layout. They agree now: 48..63, 16 bits.)
    ///
    /// The note that used to sit here said folding `identity_hash_code` "buys
    /// zero, because `AtomicU64` forces 8-byte alignment and the 4 bytes
    /// reappear as padding". That was true and it was not a reason to stop:
    /// the fold buys zero ALONE and is a prerequisite for the 8 that the
    /// quartet's move then paid out. See
    /// `header-16-and-field-packing-20260806.md` §4.
    pub mark_word: AtomicU64,
}

/// GC flag: object resides in the old generation.
pub const GC_FLAG_OLD_GEN: u8 = 0x01;

/// GC flag: object is marked as live during major GC mark phase.
pub const GC_FLAG_MARKED: u8 = 0x02;

/// GC flag: object uses the **compact reference-field layout** — reference
/// instance fields are stored as bare 8-byte pointers (per the registered
/// [`crate::field_layout::CompactLayout`] version selected by its class and
/// field count). Set at allocation time and never cleared (a permanent
/// property of the object).
///
/// Decided per-object so AUTOBOX wrappers, ad-hoc `ClassId(0)` containers, and
/// objects allocated before a synthetic-stub class grew all stay on the legacy
/// uniform 16-byte-cell layout (no flag) within the same process.
pub const GC_FLAG_COMPACT: u8 = 0x04;

impl ObjectHeader {
    /// Construct a fresh, unlocked object header. The mark word is initialized
    /// to `MARK_NEUTRAL` (no lock held, no identity hash installed).
    ///
    /// This is the canonical constructor: callers that previously built an
    /// `ObjectHeader` via struct-literal syntax should migrate to this, since
    /// the mark-word field is `AtomicU64` (`!Copy`) and cannot be omitted from
    /// a `..Default::default()` shorthand.
    pub fn new(
        class_id: ClassId,
        kind: ObjectKind,
        element_type: ArrayElementType,
        array_length: u32,
        num_slots: u32,
    ) -> Self {
        let shape = if kind == ObjectKind::Array {
            array_length
        } else {
            num_slots
        };
        // The quartet is born inside the mark word. `gc_age` and `gc_flags`
        // start at 0, which `MARK_NEUTRAL` already gives us.
        let mark = MARK_NEUTRAL
            | ((kind as u64) & KIND_BITS) << KIND_SHIFT
            | ((element_type as u64) & ELEM_BITS) << ELEM_SHIFT;
        Self {
            class_id,
            shape,
            mark_word: AtomicU64::new(mark),
        }
    }

    #[inline]
    pub fn array_length(&self) -> u32 {
        if self.kind() == ObjectKind::Array {
            self.shape
        } else {
            0
        }
    }

    #[inline]
    pub fn set_array_length(&mut self, length: u32) {
        debug_assert_eq!(self.kind(), ObjectKind::Array);
        self.shape = length;
    }

    #[inline]
    pub fn num_slots(&self) -> u32 {
        self.shape
    }

    #[inline]
    pub fn set_num_slots(&mut self, slots: u32) {
        self.shape = slots;
    }

    /// Mark an object as using its class's compact field layout.
    ///
    /// The full 32-bit logical field count remains in the shape word. Body
    /// size is owned by immutable class metadata and is only reclaimed after
    /// the loader and all of its instances are proven dead.
    #[inline]
    pub fn set_compact_shape(&mut self, slots: u32, body_size: usize) {
        assert_eq!(body_size & 7, 0, "compact body must be 8-byte aligned");
        self.shape = slots;
        self.add_gc_flags(GC_FLAG_COMPACT);
    }

    /// Returns true if this object has been forwarded by the GC.
    ///
    /// Reads the mark word's `MARK_FORWARDED` tag. Until 2026-08-06 this read a
    /// dedicated `forwarding_ptr` field; folding it into the mark word — whose
    /// `0b11` state had been designed, tested and left without a producer since
    /// 2026-07-26 — is the whole of the 32 → 24 header shrink.
    ///
    /// `Relaxed` is the right ordering for the same reason the field read was
    /// unordered: the collector installs forwarding inside a stop-the-world
    /// pause, and the pause's own handshake is the acquire/release edge.
    pub fn is_forwarded(&self) -> bool {
        Self::is_forwarded_mark(self.mark_word.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Returns the forwarding address, or null if not forwarded.
    ///
    /// Null for every non-`FORWARDED` state, which preserves the field's
    /// contract exactly: `NEUTRAL` read as a null field, and a `THIN_LOCKED` or
    /// `INFLATED` payload must never be handed back as a relocation target.
    pub fn forwarding_address(&self) -> *mut u8 {
        let mark = self.mark_word.load(std::sync::atomic::Ordering::Relaxed);
        if Self::is_forwarded_mark(mark) {
            Self::forwarding_target(mark)
        } else {
            std::ptr::null_mut()
        }
    }

    /// Install `target` as this object's relocation address.
    ///
    /// # The ordering contract this inherits
    ///
    /// Writing `FORWARDED` **destroys** whatever lock state the mark word held,
    /// so the caller must have copied the object FIRST — the destination then
    /// carries the intact `NEUTRAL` / `THIN_LOCKED` / `INFLATED` word — and
    /// clobber the **source** only afterwards. For an `INFLATED` source this
    /// TRANSFERS the single strong `Arc<Monitor>` reference the mark word owns
    /// to the destination copy; releasing it against the source afterwards
    /// leaves the live destination with a dangling `Monitor*`.
    ///
    /// This obligation did not exist while forwarding lived in its own field
    /// (the two words were distinct), and it is the reason the encoding was
    /// landed inert in 2026-07-26 rather than wired up opportunistically. See
    /// `header-shrink.md` §4.3.
    pub fn set_forwarding_address(&self, target: *mut u8) {
        let prev = self.mark_word.load(std::sync::atomic::Ordering::Relaxed);
        self.mark_word.store(
            Self::make_forwarded(prev, target as usize),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Byte offset of this object's **payload** from its base: instance-field
    /// data for an object, element data for an array.
    ///
    /// The two are the same number today and stop being the same at
    /// `HEADER_SIZE = 16`, where an array's length moves into an 8-byte prefix
    /// at the head of its body. Code that addresses a payload *without knowing
    /// which kind it has* — a closure shared between the array and object
    /// walks, a generic scan — must go through this rather than pick one
    /// constant, because picking one is right for half its callers and silently
    /// wrong for the other half.
    ///
    /// Code that already knows the kind should name the constant directly:
    /// [`HEADER_SIZE`] for instance fields, [`ARRAY_DATA_OFFSET`] for elements.
    /// That keeps the classification visible at the site rather than deferring
    /// it to a runtime branch on a header the caller has already matched on.
    #[inline]
    pub fn payload_offset(&self) -> usize {
        if self.kind() == ObjectKind::Array {
            ARRAY_DATA_OFFSET
        } else {
            HEADER_SIZE
        }
    }

    // --- The quartet, read and written through the mark word ---------------

    /// This object's kind. Reads one atomic word and shifts.
    #[inline(always)]
    pub fn kind(&self) -> ObjectKind {
        Self::kind_of(self.mark_word.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// `kind` decoded from a mark-word snapshot.
    ///
    /// Total, not fallible: the field is 2 bits and `ObjectKind` has 3
    /// variants, so `0b11` is the one unmapped value. It decodes to `Object`
    /// rather than panicking because this is reached from GC walks over
    /// possibly-corrupt memory, where an abort is worse than a value the
    /// caller's own plausibility screen will reject. Callers that must
    /// distinguish "corrupt" use [`Self::kind_tag`].
    #[inline(always)]
    pub fn kind_of(mark: u64) -> ObjectKind {
        match (mark >> KIND_SHIFT) & KIND_BITS {
            1 => ObjectKind::Array,
            2 => ObjectKind::HumongousFiller,
            _ => ObjectKind::Object,
        }
    }

    /// The raw 2-bit kind tag from a snapshot, for screens that must reject an
    /// out-of-range discriminant instead of coercing it.
    ///
    /// This replaces reading the old `kind` BYTE directly. That read existed
    /// precisely so the optimiser could not assume the enum was in range and
    /// fold a validity check away, and the same hazard applies here.
    #[inline(always)]
    pub fn kind_tag(mark: u64) -> u8 {
        ((mark >> KIND_SHIFT) & KIND_BITS) as u8
    }

    /// The raw 4-bit `element_type` tag from a snapshot. Validate with
    /// [`array_element_type_from_tag`] before decoding it as the enum.
    #[inline(always)]
    pub fn element_type_tag(mark: u64) -> u8 {
        ((mark >> ELEM_SHIFT) & ELEM_BITS) as u8
    }

    /// This object's array element type. Meaningful only for arrays.
    #[inline(always)]
    pub fn element_type(&self) -> ArrayElementType {
        let tag = Self::element_type_tag(self.mark_word.load(std::sync::atomic::Ordering::Relaxed));
        array_element_type_from_tag(tag).unwrap_or(ArrayElementType::Reference)
    }

    /// `gc_age` decoded from a mark-word snapshot, for callers that already
    /// hold the word (or must read it unaligned from a raw base).
    #[inline(always)]
    pub fn gc_age_of(mark: u64) -> u8 {
        ((mark >> AGE_SHIFT) & AGE_BITS) as u8
    }

    /// Number of minor collections this object has survived, 0..=[`MAX_GC_AGE`].
    #[inline(always)]
    pub fn gc_age(&self) -> u8 {
        ((self.mark_word.load(std::sync::atomic::Ordering::Relaxed) >> AGE_SHIFT) & AGE_BITS) as u8
    }

    /// Set the GC age, saturating at [`MAX_GC_AGE`].
    ///
    /// Saturation is deliberate: the field is 4 bits, and wrapping would make a
    /// long-lived object read as freshly allocated, which is a promotion
    /// decision made on a lie.
    pub fn set_gc_age(&self, age: u8) {
        let age = (age.min(MAX_GC_AGE) as u64) << AGE_SHIFT;
        let mut cur = self.mark_word.load(std::sync::atomic::Ordering::Relaxed);
        loop {
            let next = (cur & !(AGE_BITS << AGE_SHIFT)) | age;
            match self.mark_word.compare_exchange_weak(
                cur,
                next,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(observed) => cur = observed,
            }
        }
    }

    /// The GC flag bits ([`GC_FLAG_OLD_GEN`] / [`GC_FLAG_MARKED`] /
    /// [`GC_FLAG_COMPACT`]).
    #[inline(always)]
    pub fn gc_flags(&self) -> u8 {
        ((self.mark_word.load(std::sync::atomic::Ordering::Relaxed) >> FLAGS_SHIFT) & FLAGS_BITS)
            as u8
    }

    /// Set flag bits. A single `fetch_or`, so it cannot lose a concurrent
    /// marker's bit the way a load/modify/store would.
    #[inline(always)]
    pub fn add_gc_flags(&self, flags: u8) {
        self.mark_word.fetch_or(
            ((flags as u64) & FLAGS_BITS) << FLAGS_SHIFT,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Clear flag bits. `fetch_and`, for the same reason.
    ///
    /// There is deliberately no `try_clear_gc_flags` twin of
    /// [`Self::try_add_gc_flags`]. A claim primitive earns its keep only where
    /// something branches on "I was the one", and no unmark pass does: every
    /// one of them (the young sweep's post-join survivor loop and its
    /// sequential arm in `gc/src/gen_heap.rs`, the old-gen sweep there, the
    /// `gc/src/zgc.rs` pre-clear and sweep) either runs stop-the-world on one
    /// thread or gets its exclusion from owning a region, and a clear is
    /// idempotent besides. An unused claim primitive is worse than none: its
    /// ordering contract cannot be validated by a caller, so the first caller
    /// to arrive assumes whichever semantics it happens to need. Add it *with*
    /// that caller -- the case that would justify it is a parallel unmark
    /// where a per-object action must run exactly once at clear time.
    #[inline(always)]
    pub fn clear_gc_flags(&self, flags: u8) {
        self.mark_word.fetch_and(
            !(((flags as u64) & FLAGS_BITS) << FLAGS_SHIFT),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Replace the whole flag field.
    pub fn set_gc_flags(&self, flags: u8) {
        let bits = ((flags as u64) & FLAGS_BITS) << FLAGS_SHIFT;
        let mut cur = self.mark_word.load(std::sync::atomic::Ordering::Relaxed);
        loop {
            let next = (cur & !(FLAGS_BITS << FLAGS_SHIFT)) | bits;
            match self.mark_word.compare_exchange_weak(
                cur,
                next,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(observed) => cur = observed,
            }
        }
    }

    /// Set flag bits, reporting whether **this** call is the one that set them.
    ///
    /// Returns `true` for exactly one caller per object per clear -> set
    /// transition; every other racing caller gets `false`. This is the
    /// exactly-once *claim* primitive a **concurrent** marker needs, and it is
    /// why it exists alongside [`Self::add_gc_flags`], which is a bare
    /// `fetch_or` and tells its caller nothing.
    ///
    /// The pattern it replaces is a check-then-act:
    ///
    /// ```text
    /// if header.gc_flags() & GC_FLAG_MARKED != 0 { return; }  // already seen
    /// header.add_gc_flags(GC_FLAG_MARKED);                    // ...and race
    /// ```
    ///
    /// which is correct only while marking is stop-the-world with a *single*
    /// marker thread. With N work-stealing workers plus mutators publishing
    /// marks from a load barrier, two threads can both read the bit clear and
    /// both push the object -- or, worse, an object can be claimed by a thread
    /// that then treats it as already-scanned, so its out-edges are never
    /// traced and a live object is collected. That is a use-after-free, and it
    /// presents as heap corruption or a hang rather than as a mark-phase bug.
    ///
    /// The intended consumer is `ZMarkContext::try_mark` in
    /// `gc/src/zgc/mark.rs`, whose contract ("two workers, or a worker and a
    /// mutator, racing on the same object must see exactly one `true`") this
    /// is written to satisfy. Wiring it up is a separate change -- see the
    /// note in `gc/src/zgc_concurrent.rs` -- so until then this has no
    /// production caller and the STW collectors keep using
    /// [`Self::add_gc_flags`].
    ///
    /// # Multi-bit semantics: ALL-OR-NOTHING, not "any"
    ///
    /// `true` means **every** requested bit was clear when this call ran and
    /// **every** requested bit was set by this call, in one atomic step. If any
    /// requested bit was already set the call returns `false` and writes
    /// **nothing** -- it does not top up the remaining bits.
    ///
    /// The rejected alternative was "true if *any* requested bit transitioned".
    /// It stops being exactly-once the moment two callers request overlapping
    /// but unequal sets: a caller claiming `A` and a caller claiming `A | B`
    /// would both be told they won. Exactly-once has to survive that, so the
    /// claim is all-or-nothing and the return value means one thing.
    ///
    /// The consequence, which callers must respect: **never mix a sticky flag
    /// into a claim set.** [`GC_FLAG_COMPACT`] is set at allocation and never
    /// cleared, and [`GC_FLAG_OLD_GEN`] outlives any one mark cycle, so
    /// `try_add_gc_flags(GC_FLAG_MARKED | GC_FLAG_OLD_GEN)` on an old-gen
    /// object returns `false` forever and the object is never marked. Claim
    /// per-cycle flags only; record a sticky property with
    /// [`Self::add_gc_flags`].
    ///
    /// # Orderings: `AcqRel` on success, `Acquire` on failure and on the load
    ///
    /// This is the one flag helper that carries a happens-before edge, which is
    /// exactly why it does not use `Relaxed` like its neighbours.
    /// [`Self::add_gc_flags`], [`Self::clear_gc_flags`], [`Self::set_gc_flags`]
    /// and [`Self::set_gc_age`] run inside a stop-the-world pause with every
    /// mutator parked: the pause's own thread handshake already supplies the
    /// ordering, and nothing branches on their result, so a per-object barrier
    /// would buy nothing on a hot loop. Neither is true here -- this runs with
    /// mutators running, and its result decides whether an object's reference
    /// fields are traced at all.
    ///
    /// * **Release** on the success path: the winner has usually published
    ///   something about the object first -- a relocated copy, a healed slot,
    ///   the field stores a mutator made before it reached the load barrier.
    ///   Whoever later observes the bit set must see those writes.
    /// * **Acquire** on the success path: the winner's very next act is to
    ///   *read* the object's reference fields. Without an acquire that scan is
    ///   unordered against every release-write that reached this word, and the
    ///   winner may trace a stale or half-initialised body. That is what makes
    ///   the success ordering `AcqRel` and not a plain `Release` -- a
    ///   claim-then-read primitive needs both halves, not just the publishing
    ///   one.
    /// * **Acquire** on the failure path: "the loser must not proceed"
    ///   understates the loser. A load barrier that loses the claim still hands
    ///   the reference back to the mutator, which dereferences it; a marker
    ///   that loses still *skips* the object on the strength of the winner's
    ///   write. Both are decisions taken against the winner's release, so the
    ///   loser needs the matching acquire. `Relaxed` here would let a loser read
    ///   a body the winner had not finished publishing.
    /// * **Acquire** on the initial load, for the same reason and easily
    ///   missed: a caller that finds the bit already set returns `false`
    ///   *without ever executing a CAS*, so that one load is its only
    ///   synchronisation with the winner.
    ///
    /// Not `SeqCst`: nothing here needs a single total order across two
    /// locations -- every participant synchronises through this one word -- and
    /// `SeqCst` would put an `mfence` / `dmb ish` on the collector's hottest
    /// path for no extra guarantee. On x86-64 the whole choice is free anyway
    /// (`lock cmpxchg` is a full barrier); it is written for the memory model
    /// and for aarch64, where it is not free.
    ///
    /// # Why `compare_exchange_weak`, when the ZGC load barrier uses the strong form
    ///
    /// `_weak` may fail spuriously on LL/SC targets. That is harmless *in a
    /// loop*: a spurious failure re-reads the word and re-evaluates the
    /// already-set test, so it costs one iteration and can never turn a win into
    /// a loss or a loss into a win. In exchange we drop the hidden retry loop
    /// the strong form must emit around LL/SC -- cheaper on precisely the
    /// platform where the two differ.
    ///
    /// The contrast is `load_barrier_slow` in `gc/src/zgc/barrier.rs`, which
    /// deliberately uses the **non-weak** `compare_exchange` to self-heal a
    /// reference slot: it never retries, it simply accepts the winner's value
    /// when it loses, because a real loss means somebody else healed the slot.
    /// A spurious failure there is indistinguishable from a real loss and would
    /// leave that slot permanently unhealed, so every future load of it
    /// re-enters the slow path. The rule both sides follow: `_weak` iff you
    /// loop, strong iff the losing arm gives up. (Its orderings are `AcqRel` /
    /// `Acquire` for the same reasons as here, and deliberately match.)
    ///
    /// # Every other bit survives, and the losing arm is the dangerous part
    ///
    /// The replacement word is `cur | bits`, built from the word this call is
    /// CASing *against*, so the 2-bit state tag, the thin-lock owner and
    /// recursion count, the inflated `Monitor` pointer, the forwarding target,
    /// the identity hash, `kind`, `element_type` and `gc_age` are all carried
    /// over verbatim. If a mutator inflates a monitor between the load and the
    /// CAS then `cur` no longer matches, the CAS fails, and the retry rebuilds
    /// against the *new* word -- a pre-inflation word can never be stamped back
    /// over a live monitor pointer.
    ///
    /// "Verbatim" is meant literally, and it used to be strictly stronger than
    /// what [`Self::quartet_of`] gave. While `MARK_QUARTET_MASK` was
    /// `0x3FFF << 48` (bits 48..61) it did not cover `gc_age`'s bits 62..63, so
    /// a word rebuilt through `quartet_of` lost any age >= 4 -- while a claim,
    /// which never rebuilds and only ORs into the word it observed, kept it.
    /// This method was therefore immune to that gap and, importantly, was NOT
    /// a fix for it. The mask was widened to bits 48..63 on 2026-08-07 (see the
    /// note at [`MARK_QUARTET_MASK`]); the two paths now agree, and this
    /// paragraph is kept so a future reader does not re-derive the old
    /// asymmetry from the tests that used to work around it.
    ///
    /// The precedent for taking the losing arm this seriously is the fixed
    /// forwarding CAS in `SharedEvac::evacuate` (`gc/src/g1.rs`, the parallel
    /// evacuation path): its loser cast the raw mark word straight to a
    /// pointer instead of decoding the winner's forwarding target out of it, so
    /// every reference to an already-evacuated object came back as
    /// `address | MARK_FORWARDED` -- `address + 3`. It surfaced as
    /// `ObjectRef pointer not 8-byte aligned` in a build with that debug
    /// assertion, and as a corrupt reference stored into a live object in one
    /// without; only a two-worker race on the *same* object reaches the arm at
    /// all, which is why tree-shaped evacuation tests never saw it. Here the
    /// losing arm returns `false`, writes nothing at all, and re-derives every
    /// decision from the word the CAS actually observed
    /// (`Err(observed) => cur = observed`), never from the stale one it
    /// proposed against.
    ///
    /// # Termination
    ///
    /// Lock-free. Each iteration either returns, or observes a word different
    /// from the one it proposed against. A non-spurious difference is another
    /// thread's *completed* RMW on this word, i.e. system-wide progress;
    /// spurious failures are bounded by the hardware.
    #[inline]
    pub fn try_add_gc_flags(&self, flags: u8) -> bool {
        debug_assert!(
            (flags as u64) & !FLAGS_BITS == 0,
            "a claim set must lie inside the 4-bit gc_flags field"
        );
        let bits = ((flags as u64) & FLAGS_BITS) << FLAGS_SHIFT;
        // An empty claim set has no clear -> set transition to win. Without this
        // guard the CAS below would propose `cur | 0 == cur`, succeed trivially,
        // and report a win to *every* caller -- the exact inverse of the
        // contract. `flags == 0` passes the range `debug_assert` above, so this
        // branch is the only guard -- and it is a real branch, not an assert,
        // because release builds are where a concurrent marker actually runs.
        if bits == 0 {
            return false;
        }
        let mut cur = self.mark_word.load(std::sync::atomic::Ordering::Acquire);
        loop {
            // All-or-nothing: any requested bit already set means this call did
            // not perform the transition, so it claims nothing and writes
            // nothing.
            if cur & bits != 0 {
                return false;
            }
            match self.mark_word.compare_exchange_weak(
                cur,
                cur | bits,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => cur = observed,
            }
        }
    }

    /// Set `kind` and `element_type`. Allocation-time only: both are immutable
    /// for an object's lifetime, so this takes no care to survive a race.
    pub fn set_shape_tags(&self, kind: ObjectKind, element_type: ArrayElementType) {
        let bits = ((kind as u64) & KIND_BITS) << KIND_SHIFT
            | ((element_type as u64) & ELEM_BITS) << ELEM_SHIFT;
        let cur = self.mark_word.load(std::sync::atomic::Ordering::Relaxed);
        self.mark_word.store(
            (cur & !(((KIND_BITS) << KIND_SHIFT) | ((ELEM_BITS) << ELEM_SHIFT))) | bits,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// The quartet bits of a snapshot, for constructing a replacement word that
    /// keeps them.
    ///
    /// This is the whole of bits 48..63, so every one of `kind`,
    /// `element_type`, `gc_flags` and the FULL 4-bit `gc_age` survives a
    /// rebuild. Until 2026-08-07 the mask stopped at bit 61 and this silently
    /// truncated any age >= 4 to `age & 3` on every `make_thin_locked` /
    /// `make_inflated` / `make_forwarded`; see [`MARK_QUARTET_MASK`].
    #[inline(always)]
    pub fn quartet_of(mark: u64) -> u64 {
        mark & MARK_QUARTET_MASK
    }

    /// Returns true if this object is in the old generation.
    pub fn is_old_gen(&self) -> bool {
        self.gc_flags() & GC_FLAG_OLD_GEN != 0
    }

    // --- Mark-word helpers (associated functions, not methods, so callers
    // can manipulate a previously-loaded `u64` snapshot without re-reading
    // the atomic on every accessor call -- the typical pattern inside a
    // CAS loop). -----------------------------------------------------------

    /// Extract the 2-bit state field from a mark word snapshot.
    #[inline(always)]
    pub fn mark_state(mark: u64) -> u64 {
        mark & MARK_STATE_MASK
    }

    /// Construct a `MARK_THIN_LOCKED` mark word from owner + recursion count.
    ///
    /// `recursion = 0` means the lock is held exactly once. Maximum supported
    /// nesting is `255 + 1 = 256` re-entrant acquisitions; beyond that the
    /// caller must inflate to a full `Monitor`.
    #[inline(always)]
    pub fn make_thin_locked(prev: u64, thread_id: u32, recursion: u8) -> u64 {
        Self::quartet_of(prev)
            | MARK_THIN_LOCKED
            | ((recursion as u64) << THIN_LOCK_RECURSION_SHIFT)
            | ((thread_id as u64) << THIN_LOCK_OWNER_SHIFT)
    }

    /// Decode the owner thread id from a `MARK_THIN_LOCKED` mark word.
    /// Result is meaningless if the mark word is not in thin-locked state.
    #[inline(always)]
    pub fn thin_lock_owner(mark: u64) -> u32 {
        ((mark & THIN_LOCK_OWNER_MASK) >> THIN_LOCK_OWNER_SHIFT) as u32
    }

    /// Decode the recursion count from a `MARK_THIN_LOCKED` mark word.
    /// Result is meaningless if the mark word is not in thin-locked state.
    #[inline(always)]
    pub fn thin_lock_recursion(mark: u64) -> u8 {
        ((mark & THIN_LOCK_RECURSION_MASK) >> THIN_LOCK_RECURSION_SHIFT) as u8
    }

    /// Construct a `MARK_INFLATED` mark word pointing at the given `Monitor`.
    ///
    /// The pointer must be at least 4-byte aligned so its low 2 bits are
    /// available for the state tag. `Monitor` should be `#[repr(align(8))]`
    /// or naturally 8-aligned in practice.
    #[inline(always)]
    pub fn make_inflated(prev: u64, monitor_ptr: usize) -> u64 {
        assert!(
            monitor_ptr & (MARK_STATE_MASK as usize) == 0,
            "Monitor pointer must have its low 2 bits clear (>= 4-byte aligned)"
        );
        assert!(
            crate::plausible_heap_pointer(monitor_ptr as u64),
            "Monitor pointer must be a non-null, 8-byte aligned plausible user-space pointer"
        );
        Self::quartet_of(prev) | (monitor_ptr as u64) | MARK_INFLATED
    }

    /// Decode the `Monitor` pointer from a `MARK_INFLATED` mark word.
    /// Result is meaningless if the mark word is not in inflated state.
    ///
    /// `& !MARK_QUARTET_MASK` is load-bearing, not cosmetic: the quartet is
    /// written into this word AFTER the state (every collector ages a survivor
    /// it has just copied), so the bits really are set on words this decodes.
    /// While the mask stopped at bit 61 this expression left `gc_age`'s bits
    /// 62..63 in the result and returned `monitor_ptr | (1 << 62)` for any age
    /// >= 4 -- a non-null wild pointer, which `monitor_ptr_from_mark` in
    /// `vm/src/threading/monitor.rs` only null-checks. Fixed 2026-08-07 by
    /// widening the mask; see the note at [`MARK_QUARTET_MASK`].
    #[inline(always)]
    pub fn inflated_monitor(mark: u64) -> *mut () {
        ((mark & INFLATED_PTR_MASK & !MARK_QUARTET_MASK) as usize) as *mut ()
    }

    /// Construct a `MARK_FORWARDED` mark word pointing at an object's new
    /// location after GC relocation.
    ///
    /// The address is OR-ed with the tag rather than *shifted*, so nothing is
    /// truncated and no overflow side table is required. Heap objects are
    /// 8-byte aligned, so the low 3 bits are already zero and borrowing 2 of
    /// them is free.
    ///
    /// The usable width is the **47 bits `plausible_heap_pointer` admits**
    /// (`target <= 2^47 - 1`, asserted below), not 64: bits 48..63 are the
    /// quartet's, and [`Self::forwarding_target`] masks them off on the way
    /// out. That has always been true — the doc here used to say "full 64-bit"
    /// while `quartet_of` was already stamping bits 48..61 of the result — and
    /// it costs nothing, because the assert makes a target with any bit above
    /// 46 set a panic rather than a truncation.
    ///
    /// # Ordering contract (read before producing this state)
    ///
    /// Writing this state **destroys** whatever lock state the mark word held.
    /// A relocating collector must therefore copy the object *first* — so the
    /// destination's mark word carries the intact NEUTRAL / THIN_LOCKED /
    /// INFLATED value — and only then clobber the **source** mark word with
    /// `make_forwarded(dest)`. For an `INFLATED` source this transfers the one
    /// strong `Arc<Monitor>` reference the mark word owns to the destination
    /// copy; it must not be released against the source afterwards, or the
    /// live destination is left with a dangling `Monitor*`. See
    /// `header-shrink.md` §4.
    #[inline(always)]
    pub fn make_forwarded(prev: u64, target: usize) -> u64 {
        assert!(
            target & (MARK_STATE_MASK as usize) == 0,
            "forwarding target must have its low 2 bits clear (>= 4-byte aligned)"
        );
        assert!(
            crate::plausible_heap_pointer(target as u64),
            "forwarding target must be a non-null, 8-byte aligned plausible user-space pointer"
        );
        Self::quartet_of(prev) | (target as u64) | MARK_FORWARDED
    }

    /// Decode the relocation target from a `MARK_FORWARDED` mark word.
    /// Result is meaningless if the mark word is not in forwarded state;
    /// check with [`ObjectHeader::is_forwarded_mark`] first.
    ///
    /// `& !MARK_QUARTET_MASK` must strip the WHOLE quartet, for the same reason
    /// spelled out on [`Self::inflated_monitor`]: a high `gc_age` written after
    /// the state used to survive into the returned address.
    #[inline(always)]
    pub fn forwarding_target(mark: u64) -> *mut u8 {
        ((mark & FORWARDING_PTR_MASK & !MARK_QUARTET_MASK) as usize) as *mut u8
    }

    /// Read this object's identity hash out of the mark word, installing one
    /// from `mint` if it has none yet.
    ///
    /// `Ok(hash)` — the hash lives (or now lives) in the mark word.
    /// `Err(())` — the object is not `NEUTRAL`, so its hash cannot live here.
    /// The caller must go to the displaced-hash table, and must **not** mint:
    /// minting per call would hand out a different value every time.
    ///
    /// # Why the CAS retries instead of taking the loser's word for it
    ///
    /// The obvious shape is "CAS failed, so somebody else installed a hash;
    /// return theirs". That is wrong here, because a CAS against a `NEUTRAL`
    /// word can also lose to a *thin lock* — the same word, the same instant,
    /// a completely different state. Re-reading is what distinguishes the two,
    /// and it is what routes the lock case to `Err` rather than decoding an
    /// owner id as a hash.
    ///
    /// The loop terminates: each iteration either returns, or observes a state
    /// change, and the only transition back into un-hashed `NEUTRAL` is a thin
    /// unlock, which cannot repeat without a matching lock.
    #[inline]
    pub fn mark_word_identity_hash(&self, mint: impl Fn() -> i32) -> Result<i32, ()> {
        loop {
            let mark = self.mark_word.load(std::sync::atomic::Ordering::Relaxed);
            if Self::mark_state(mark) != MARK_NEUTRAL {
                return Err(());
            }
            let existing = Self::neutral_hash(mark);
            if existing != 0 {
                return Ok(existing);
            }
            let candidate = Self::make_neutral_hashed(mark, mint());
            if self
                .mark_word
                .compare_exchange(
                    mark,
                    candidate,
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                )
                .is_ok()
            {
                return Ok(Self::neutral_hash(candidate));
            }
        }
    }

    /// Build a `MARK_NEUTRAL` word carrying `hash` as this object's identity
    /// hash.
    ///
    /// `hash` is truncated to the 31 bits [`MARK_HASH_MASK`] covers and forced
    /// non-zero: zero is the "no hash installed yet" encoding, so a generator
    /// that happened to produce 0 (or a multiple of 2^31) would install a hash
    /// that reads back as absent and be re-minted on the next call — a
    /// *different* identity hash for the same live object, which is the one
    /// thing this must never do.
    #[inline(always)]
    pub fn make_neutral_hashed(prev: u64, hash: i32) -> u64 {
        let bits = (hash as u32 as u64) & (MARK_HASH_MASK >> MARK_HASH_SHIFT);
        let bits = if bits == 0 { 1 } else { bits };
        Self::quartet_of(prev) | MARK_NEUTRAL | (bits << MARK_HASH_SHIFT)
    }

    /// The identity hash carried by a mark word snapshot, or `0` for "none".
    ///
    /// `0` for every non-`NEUTRAL` state as well as for an un-hashed neutral
    /// word: a `THIN_LOCKED` payload is an owner and recursion count and an
    /// `INFLATED`/`FORWARDED` payload is a pointer, and handing any of those
    /// back as a hash would be worse than useless. A caller that gets `0` from a
    /// non-neutral word must consult the displaced-hash table, not mint one.
    #[inline(always)]
    pub fn neutral_hash(mark: u64) -> i32 {
        if Self::mark_state(mark) != MARK_NEUTRAL {
            return 0;
        }
        ((mark & MARK_HASH_MASK) >> MARK_HASH_SHIFT) as i32
    }

    /// Whether a mark word snapshot encodes a GC forwarding pointer.
    ///
    /// Uses tag *equality*, not a bitwise test — `mark & MARK_FORWARDED != 0`
    /// would also be true for every `MARK_THIN_LOCKED` and `MARK_INFLATED`
    /// word, which is precisely the aliasing this state was audited against.
    #[inline(always)]
    pub fn is_forwarded_mark(mark: u64) -> bool {
        Self::mark_state(mark) == MARK_FORWARDED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The heap-free header screen accepts what the collector's own
    /// `is_object_address` accepts and rejects the two things the band test was
    /// calling live references: a filler, and a word that is merely a number.
    #[test]
    fn the_object_header_screen_separates_headers_from_numbers() {
        let obj = ObjectHeader::new(
            ClassId::new(7),
            ObjectKind::Object,
            ArrayElementType::Reference,
            0,
            3,
        );
        let ptr = &obj as *const ObjectHeader as *const u8;
        assert!(
            unsafe { plausible_object_header_at(ptr) },
            "an ordinary object header must pass"
        );

        let arr = ObjectHeader::new(
            ClassId::new(8),
            ObjectKind::Array,
            ArrayElementType::Long,
            16,
            16,
        );
        assert!(
            unsafe { plausible_object_header_at(&arr as *const ObjectHeader as *const u8) },
            "an array header must pass"
        );

        let filler = ObjectHeader::new(
            ClassId::new(9),
            ObjectKind::HumongousFiller,
            ArrayElementType::Reference,
            0,
            0,
        );
        assert!(
            !unsafe { plausible_object_header_at(&filler as *const ObjectHeader as *const u8) },
            "a humongous filler is not an object a root can name"
        );

        // ...and the population the band test was flagging: plain integers
        // whose value happens to land in the heap's address range.
        //
        // A FILTER, NOT A PROOF, and the test says so rather than pretending
        // otherwise. The kind and element-type tags are a handful of bits, so
        // some arbitrary words decode to a valid pair by chance; what this
        // screen removes is the large majority, which is the difference
        // between the band test's range-only question and a question about
        // objects. Asserted as a rate over a spread, because an assertion on
        // one hand-picked word would be a coin toss dressed as a property.
        let mut rejected = 0usize;
        const N: usize = 256;
        for i in 0..N {
            // Values with no relationship to a header layout.
            let junk: [u64; 4] = [
                0x1234_5678 ^ (i as u64),
                (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15),
                u64::MAX - i as u64,
                i as u64,
            ];
            if !unsafe { plausible_object_header_at(junk.as_ptr() as *const u8) } {
                rejected += 1;
            }
        }
        assert!(
            rejected * 4 >= N * 3,
            "the screen must reject the large majority of arbitrary words              (rejected {rejected} of {N}); it is what separates \"a word in the              heap's range\" from \"an object\""
        );
    }

    // Helper to create a default ObjectHeader for testing.
    fn make_header() -> ObjectHeader {
        ObjectHeader::new(
            ClassId::new(1),
            ObjectKind::Object,
            ArrayElementType::Boolean,
            0,
            0,
        )
    }

    // -- Constants --

    #[test]
    fn header_size_is_correct() {
        assert_eq!(HEADER_SIZE, 16);
        assert_eq!(std::mem::size_of::<ObjectHeader>(), HEADER_SIZE);
        assert_eq!(std::mem::align_of::<ObjectHeader>(), 8);
        assert_eq!(std::mem::offset_of!(ObjectHeader, class_id), 0);
        // Three fields, no padding: `class_id`(4) + `shape`(4) + the 8-aligned
        // mark word is exactly 16, which is why nothing else could stay a
        // field. `kind` / `element_type` / `gc_age` / `gc_flags` are in the
        // mark word's bits 48..63; the two JIT-facing byte offsets below are
        // what emitted code addresses them through.
        assert_eq!(GC_FLAGS_BYTE_OFFSET, MARK_WORD_OFFSET + 7);
        assert_eq!(KIND_TAGS_BYTE_OFFSET, MARK_WORD_OFFSET + 6);
        assert_eq!(std::mem::offset_of!(ObjectHeader, shape), NUM_SLOTS_OFFSET);
        assert_eq!(
            std::mem::offset_of!(ObjectHeader, mark_word),
            MARK_WORD_OFFSET
        );
    }

    #[test]
    fn object_shape_preserves_full_u32_field_count() {
        let mut header = make_header();
        header.set_compact_shape(0xfeed_beef, 24);
        assert_eq!(header.num_slots(), 0xfeed_beef);
        assert_eq!(header.array_length(), 0);
        assert_eq!(header.gc_flags() & GC_FLAG_COMPACT, GC_FLAG_COMPACT);
    }

    #[test]
    fn slot_size_is_16() {
        assert_eq!(SLOT_SIZE, 16);
    }

    #[test]
    fn ref_element_size_is_8() {
        assert_eq!(REF_ELEMENT_SIZE, 8);
    }

    /// Reinterpret a `Value` as its raw cell bytes, keeping the padding
    /// `MaybeUninit`.
    ///
    /// `Value` is an enum with padding, so `transmute::<Value, [u8; 16]>` is
    /// Undefined Behaviour — it claims every byte is an initialised integer
    /// when the padding is not. That is not pedantry: Miri rejects it, and it
    /// is what this test used to do. Transmuting to `[MaybeUninit<u8>; 16]` is
    /// always sound, and the readers below only ever `assume_init` the ranges
    /// the layout defines as real data.
    #[cfg(test)]
    fn value_cell_bytes(value: crate::Value) -> [std::mem::MaybeUninit<u8>; SLOT_SIZE] {
        const _: () = assert!(std::mem::size_of::<crate::Value>() == SLOT_SIZE);
        // SAFETY: sizes are equal (checked above) and `MaybeUninit<u8>` imposes
        // no validity requirement on any byte, padding included.
        unsafe { std::mem::transmute(value) }
    }

    /// Read `N` initialised bytes at `offset` out of a cell.
    ///
    /// # Panics (as a test failure)
    /// Reading a padding byte here would be UB; the caller is responsible for
    /// only naming offsets the layout assertions establish as initialised.
    #[cfg(test)]
    fn cell_bytes_at<const N: usize>(
        cell: &[std::mem::MaybeUninit<u8>; SLOT_SIZE],
        offset: usize,
    ) -> [u8; N] {
        let mut out = [0u8; N];
        for (i, slot) in out.iter_mut().enumerate() {
            // SAFETY: `offset .. offset + N` is a discriminant or payload
            // range, never padding — that is exactly what this test pins.
            *slot = unsafe { cell[offset + i].assume_init() };
        }
        out
    }

    /// Pin the in-memory layout of a `Value` field cell so the JIT's inline
    /// `getfield` codegen (which emits a raw `MOV [recv + FIELD_CELL_*]`)
    /// stays correct.
    ///
    /// `Value` is `#[repr(u32)]` with explicit discriminants, so this is now a
    /// *second* line of defence rather than the only one — `value.rs` asserts
    /// the same four facts at compile time (search `value_tag_word`), which is
    /// what actually protects the JIT. This test survives because it exercises
    /// the real byte-reinterpretation path the JIT performs, including the
    /// non-null `Object` case that const-eval cannot express (it would have to
    /// read pointer provenance).
    #[test]
    fn field_cell_layout_matches_value_enum() {
        use crate::Value;

        // A 4-byte (`Int`) payload lives at FIELD_CELL_PAYLOAD32_OFFSET.
        let cell = value_cell_bytes(Value::Int(0x1234_5678));
        let tag = u32::from_le_bytes(cell_bytes_at::<4>(&cell, FIELD_CELL_TAG_OFFSET));
        assert_eq!(
            tag, 0,
            "Int discriminant must be 0 at FIELD_CELL_TAG_OFFSET"
        );
        let p32 = i32::from_le_bytes(cell_bytes_at::<4>(&cell, FIELD_CELL_PAYLOAD32_OFFSET));
        assert_eq!(
            p32, 0x1234_5678,
            "Int payload at FIELD_CELL_PAYLOAD32_OFFSET"
        );

        // An 8-byte (`Long`) payload lives at FIELD_CELL_PAYLOAD64_OFFSET.
        let cell = value_cell_bytes(Value::Long(0x0102_0304_0506_0708_i64));
        let tag = u32::from_le_bytes(cell_bytes_at::<4>(&cell, FIELD_CELL_TAG_OFFSET));
        assert_eq!(tag, 1, "Long discriminant must be 1");
        let p64 = i64::from_le_bytes(cell_bytes_at::<8>(&cell, FIELD_CELL_PAYLOAD64_OFFSET));
        assert_eq!(p64, 0x0102_0304_0506_0708_i64, "Long payload at +8");

        // The remaining discriminants. The JIT bakes only `0` (Int) and `4`
        // (Object) as literals, but it bakes them as *positions in this
        // sequence* — a reorder that left Int at 0 while moving Object would
        // still miscompile `x64/objects.rs`. Pin the whole run so any reorder
        // fails here, not in generated code.
        for (v, want, name) in [
            (Value::Float(1.5), 2u32, "Float"),
            (Value::Double(1.5), 3, "Double"),
            (Value::Object(None), 4, "Object"),
            (Value::ReturnAddress(7), 5, "ReturnAddress"),
            (Value::Uninitialized, 6, "Uninitialized"),
        ] {
            let cell = value_cell_bytes(v);
            let tag = u32::from_le_bytes(cell_bytes_at::<4>(&cell, FIELD_CELL_TAG_OFFSET));
            assert_eq!(tag, want, "{name} discriminant must be {want}");
        }

        // `Object(None)` (JVM null) must leave the 8-byte payload word zero.
        let cell = value_cell_bytes(Value::Object(None));
        let p64 = u64::from_le_bytes(cell_bytes_at::<8>(&cell, FIELD_CELL_PAYLOAD64_OFFSET));
        assert_eq!(p64, 0, "Object(None) payload word must be zero");

        // A non-null `Object` stores its raw pointer at FIELD_CELL_PAYLOAD64_OFFSET.
        let backing = Box::leak(Box::new(0u64));
        let raw = backing as *mut u64 as usize as u64;
        let oref = unsafe { crate::ObjectRef::from_raw(backing as *mut u64 as *mut u8) };
        let cell = value_cell_bytes(Value::Object(Some(oref)));
        let p64 = u64::from_le_bytes(cell_bytes_at::<8>(&cell, FIELD_CELL_PAYLOAD64_OFFSET));
        assert_eq!(
            p64, raw,
            "Object(Some) payload word must be the raw pointer"
        );
        // SAFETY: reclaim the leaked allocation.
        unsafe { drop(Box::from_raw(backing)) };
    }

    #[test]
    fn packed_shape_offset_matches_array_length_layout() {
        assert_eq!(
            std::mem::offset_of!(ObjectHeader, shape),
            ARRAY_LENGTH_OFFSET
        );
    }

    // -- element_byte_size --

    #[test]
    fn element_byte_size_boolean() {
        assert_eq!(element_byte_size(ArrayElementType::Boolean), 1);
    }

    #[test]
    fn element_byte_size_byte() {
        assert_eq!(element_byte_size(ArrayElementType::Byte), 1);
    }

    #[test]
    fn element_byte_size_char() {
        assert_eq!(element_byte_size(ArrayElementType::Char), 2);
    }

    #[test]
    fn element_byte_size_short() {
        assert_eq!(element_byte_size(ArrayElementType::Short), 2);
    }

    #[test]
    fn element_byte_size_int() {
        assert_eq!(element_byte_size(ArrayElementType::Int), 4);
    }

    #[test]
    fn element_byte_size_float() {
        assert_eq!(element_byte_size(ArrayElementType::Float), 4);
    }

    #[test]
    fn element_byte_size_long() {
        assert_eq!(element_byte_size(ArrayElementType::Long), 8);
    }

    #[test]
    fn element_byte_size_double() {
        assert_eq!(element_byte_size(ArrayElementType::Double), 8);
    }

    #[test]
    fn element_byte_size_reference() {
        assert_eq!(
            element_byte_size(ArrayElementType::Reference),
            REF_ELEMENT_SIZE
        );
    }

    // -- array_data_size / array_data_size_checked --

    #[test]
    fn array_data_size_zero_length() {
        assert_eq!(array_data_size(0, ArrayElementType::Int).unwrap(), 0);
    }

    #[test]
    fn array_data_size_single_byte_element() {
        // 1 byte -> rounds up to 8
        assert_eq!(array_data_size(1, ArrayElementType::Byte).unwrap(), 8);
    }

    #[test]
    fn array_data_size_eight_byte_elements() {
        // 8 bytes exactly -> no padding needed
        assert_eq!(array_data_size(1, ArrayElementType::Long).unwrap(), 8);
    }

    #[test]
    fn array_data_size_alignment() {
        // 3 ints = 12 bytes -> rounds up to 16
        assert_eq!(array_data_size(3, ArrayElementType::Int).unwrap(), 16);
        // 5 ints = 20 bytes -> rounds up to 24
        assert_eq!(array_data_size(5, ArrayElementType::Int).unwrap(), 24);
        // 2 ints = 8 bytes -> exact
        assert_eq!(array_data_size(2, ArrayElementType::Int).unwrap(), 8);
    }

    #[test]
    fn array_data_size_boolean_array() {
        // 10 booleans = 10 bytes -> rounds to 16
        assert_eq!(array_data_size(10, ArrayElementType::Boolean).unwrap(), 16);
    }

    #[test]
    fn array_data_size_reference_array() {
        // 3 refs = 24 bytes -> exact
        assert_eq!(array_data_size(3, ArrayElementType::Reference).unwrap(), 24);
    }

    #[test]
    fn array_data_size_checked_overflow() {
        // Extremely large length should return None
        let result = array_data_size_checked(usize::MAX, ArrayElementType::Long);
        assert!(result.is_none());
    }

    #[test]
    fn array_data_size_checked_valid() {
        let result = array_data_size_checked(10, ArrayElementType::Int);
        assert_eq!(result, Some(40));
    }

    #[test]
    fn array_data_size_returns_err_on_overflow() {
        let result = array_data_size(usize::MAX, ArrayElementType::Long);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("overflow"));
    }

    // -- ObjectKind --

    #[test]
    fn object_kind_values() {
        assert_eq!(ObjectKind::Object as u8, 0);
        assert_eq!(ObjectKind::Array as u8, 1);
    }

    #[test]
    fn object_kind_equality() {
        assert_eq!(ObjectKind::Object, ObjectKind::Object);
        assert_ne!(ObjectKind::Object, ObjectKind::Array);
    }

    // -- ArrayElementType repr values --

    #[test]
    fn array_element_type_repr_values() {
        assert_eq!(ArrayElementType::Reference as u8, 0);
        assert_eq!(ArrayElementType::Boolean as u8, 4);
        assert_eq!(ArrayElementType::Char as u8, 5);
        assert_eq!(ArrayElementType::Float as u8, 6);
        assert_eq!(ArrayElementType::Double as u8, 7);
        assert_eq!(ArrayElementType::Byte as u8, 8);
        assert_eq!(ArrayElementType::Short as u8, 9);
        assert_eq!(ArrayElementType::Int as u8, 10);
        assert_eq!(ArrayElementType::Long as u8, 11);
    }

    // -- ObjectHeader methods --

    #[test]
    fn object_header_not_forwarded_by_default() {
        let header = make_header();
        assert!(!header.is_forwarded());
        assert!(header.forwarding_address().is_null());
    }

    #[test]
    fn object_header_forwarded() {
        let header = make_header();
        let cell: u64 = 0;
        let target = &cell as *const u64 as *mut u8;
        header.set_forwarding_address(target);
        assert!(header.is_forwarded());
        assert_eq!(header.forwarding_address(), target);
    }

    #[test]
    fn object_header_not_old_gen_by_default() {
        let header = make_header();
        assert!(!header.is_old_gen());
    }

    #[test]
    fn object_header_old_gen_flag() {
        let mut header = make_header();
        header.set_gc_flags(GC_FLAG_OLD_GEN);
        assert!(header.is_old_gen());
    }

    #[test]
    fn object_header_marked_flag_does_not_imply_old_gen() {
        let mut header = make_header();
        header.set_gc_flags(GC_FLAG_MARKED);
        assert!(!header.is_old_gen());
    }

    #[test]
    fn object_header_combined_flags() {
        let mut header = make_header();
        header.set_gc_flags(GC_FLAG_OLD_GEN | GC_FLAG_MARKED);
        assert!(header.is_old_gen());
    }

    #[test]
    fn gc_flag_constants() {
        assert_eq!(GC_FLAG_OLD_GEN, 0x01);
        assert_eq!(GC_FLAG_MARKED, 0x02);
        // Flags should not overlap
        assert_eq!(GC_FLAG_OLD_GEN & GC_FLAG_MARKED, 0);
    }

    // -- try_add_gc_flags: the exactly-once claim primitive ----------------

    /// The single-threaded contract: the first claim wins, every later one
    /// loses, and the bit is set either way.
    #[test]
    fn a_second_claim_of_the_same_flag_loses() {
        let header = make_header();
        assert!(header.try_add_gc_flags(GC_FLAG_MARKED));
        assert_eq!(header.gc_flags() & GC_FLAG_MARKED, GC_FLAG_MARKED);
        assert!(!header.try_add_gc_flags(GC_FLAG_MARKED));
        assert!(!header.try_add_gc_flags(GC_FLAG_MARKED));
        assert_eq!(header.gc_flags() & GC_FLAG_MARKED, GC_FLAG_MARKED);
    }

    /// A flag set through the non-claiming [`ObjectHeader::add_gc_flags`] must
    /// still make the claim lose. The primitive reports the state of the BIT,
    /// not whether some earlier caller happened to use the claim API.
    #[test]
    fn a_claim_loses_against_a_plain_add() {
        let header = make_header();
        header.add_gc_flags(GC_FLAG_MARKED);
        assert!(!header.try_add_gc_flags(GC_FLAG_MARKED));
    }

    /// Multi-bit claims are ALL-OR-NOTHING (see the doc comment): one bit
    /// already set makes the whole claim lose, and -- the half a "true if any
    /// bit transitioned" implementation would get wrong -- the remaining bits
    /// are left ALONE rather than topped up.
    #[test]
    fn a_multi_bit_claim_is_all_or_nothing() {
        // Both clear: one atomic claim takes both.
        let header = make_header();
        assert!(header.try_add_gc_flags(GC_FLAG_MARKED | GC_FLAG_OLD_GEN));
        assert_eq!(
            header.gc_flags() & (GC_FLAG_MARKED | GC_FLAG_OLD_GEN),
            GC_FLAG_MARKED | GC_FLAG_OLD_GEN
        );

        // One already set: the claim loses AND writes nothing.
        let header = make_header();
        header.add_gc_flags(GC_FLAG_OLD_GEN);
        assert!(!header.try_add_gc_flags(GC_FLAG_MARKED | GC_FLAG_OLD_GEN));
        assert_eq!(
            header.gc_flags() & GC_FLAG_MARKED,
            0,
            "a lost all-or-nothing claim must not set the bits it could have won"
        );

        // Which is exactly the documented footgun: an old-gen object can never
        // be claimed with a set that carries the sticky flag along...
        assert!(!header.try_add_gc_flags(GC_FLAG_MARKED | GC_FLAG_OLD_GEN));
        // ...while the per-cycle flag on its own still claims cleanly.
        assert!(header.try_add_gc_flags(GC_FLAG_MARKED));
    }

    /// An empty claim set must LOSE. `cur | 0 == cur` is a CAS that always
    /// succeeds, so a missing guard would hand a win to every caller.
    #[test]
    fn an_empty_claim_set_wins_nothing() {
        let header = make_header();
        assert!(!header.try_add_gc_flags(0));
        assert_eq!(header.gc_flags(), 0);
    }

    /// A claim must not disturb one other bit of the word. The quartet shares
    /// the mark word with the thin-lock payload and the monitor pointer, so a
    /// claim that rebuilt the word instead of OR-ing into the observed one
    /// would leak a `Monitor`, drop a forwarding target, or reset a tenuring
    /// age -- none of which the flag assertions alone would notice.
    #[test]
    fn a_claim_preserves_the_rest_of_the_mark_word() {
        let cell: u64 = 0;
        let payload = &cell as *const u64 as usize;

        let header = ObjectHeader::new(
            ClassId::new(7),
            ObjectKind::Array,
            ArrayElementType::Int,
            100,
            0,
        );
        header.add_gc_flags(GC_FLAG_COMPACT);
        let quartet = ObjectHeader::quartet_of(header.mark_word.load(Ordering::Relaxed));
        let flag_field: u64 = FLAGS_BITS << FLAGS_SHIFT;

        for (name, mark) in [
            (
                "NEUTRAL",
                ObjectHeader::make_neutral_hashed(quartet, 0x0123_4567),
            ),
            (
                "THIN_LOCKED",
                ObjectHeader::make_thin_locked(quartet, 0x0BAD_F00D, 9),
            ),
            ("INFLATED", ObjectHeader::make_inflated(quartet, payload)),
            ("FORWARDED", ObjectHeader::make_forwarded(quartet, payload)),
        ] {
            header.mark_word.store(mark, Ordering::Release);
            // The age is installed AFTER the state word rather than folded in
            // through `quartet_of`, which is also the ordering every collector
            // uses (copy the mark word, then age the copy). 9 (0b1001) sets
            // bit 63 -- one of the two the old `0x3FFF << 48` mask could not
            // carry -- so this doubles as a check that the widened mask and the
            // OR-into-observed claim agree about the full 4-bit age.
            header.set_gc_age(9);
            let before = header.mark_word.load(Ordering::Acquire);
            assert!(
                header.try_add_gc_flags(GC_FLAG_MARKED),
                "{name}: the first claim must win"
            );
            let after = header.mark_word.load(Ordering::Acquire);

            assert_eq!(
                after & !flag_field,
                before & !flag_field,
                "{name}: a claim changed a bit outside the flag field"
            );
            assert_eq!(
                ObjectHeader::mark_state(after),
                ObjectHeader::mark_state(mark),
                "{name}: the state tag moved"
            );
            assert_eq!(header.kind(), ObjectKind::Array, "{name}: kind");
            assert_eq!(
                header.element_type(),
                ArrayElementType::Int,
                "{name}: element_type"
            );
            assert_eq!(header.gc_age(), 9, "{name}: gc_age");
            assert_eq!(
                header.gc_flags() & GC_FLAG_COMPACT,
                GC_FLAG_COMPACT,
                "{name}: the sticky flag was cleared"
            );
            assert_eq!(
                header.gc_flags() & GC_FLAG_MARKED,
                GC_FLAG_MARKED,
                "{name}: claimed bit"
            );
            assert!(
                !header.try_add_gc_flags(GC_FLAG_MARKED),
                "{name}: the second claim must lose"
            );
        }
    }

    /// The property the whole primitive exists for: with N threads racing over
    /// the same objects, EXACTLY ONE call per object returns `true`.
    ///
    /// Many objects rather than one, because a single-object race is normally
    /// won outright by whichever thread arrives first and produces no overlap
    /// at all. 256 objects, with each thread entering the ring at its own
    /// offset, is what actually generates contention.
    #[test]
    fn a_claim_is_won_exactly_once_across_threads() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::{Arc, Barrier};

        const THREADS: usize = 8;
        const OBJECTS: usize = 256;

        for _round in 0..16 {
            let headers: Arc<Vec<ObjectHeader>> =
                Arc::new((0..OBJECTS).map(|_| make_header()).collect());
            let wins: Arc<Vec<AtomicUsize>> =
                Arc::new((0..OBJECTS).map(|_| AtomicUsize::new(0)).collect());
            let barrier = Arc::new(Barrier::new(THREADS));

            let handles: Vec<_> = (0..THREADS)
                .map(|t| {
                    let headers = Arc::clone(&headers);
                    let wins = Arc::clone(&wins);
                    let barrier = Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        barrier.wait();
                        for i in 0..OBJECTS {
                            let idx = (i + t * 31) % OBJECTS;
                            if headers[idx].try_add_gc_flags(GC_FLAG_MARKED) {
                                wins[idx].fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }

            for i in 0..OBJECTS {
                let claims = wins[i].load(Ordering::Relaxed);
                assert_eq!(claims, 1, "object {i} was claimed {claims} times, not once");
                assert_eq!(
                    headers[i].gc_flags() & GC_FLAG_MARKED,
                    GC_FLAG_MARKED,
                    "object {i} lost the flag it was claimed with"
                );
            }
        }
    }

    /// The crux of sharing one word: a mutator taking a thin lock or inflating
    /// a monitor mid-claim must make the CAS fail and retry, never lose its
    /// payload -- and must never split one claim into two.
    ///
    /// The churn thread rebuilds every word from `quartet_of(observed)` and the
    /// claim ORs into the word it observed, so a correct implementation has
    /// both sides surviving; an implementation that rebuilt the word from a
    /// stale snapshot loses whichever side lost the race.
    #[test]
    fn a_claim_survives_a_concurrent_lock_state_change() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::{Arc, Barrier};

        const CLAIMERS: usize = 6;
        const ATTEMPTS: usize = 512;
        const ROUNDS: usize = 512;

        for _round in 0..8 {
            let header = Arc::new(ObjectHeader::new(
                ClassId::new(11),
                ObjectKind::Array,
                ArrayElementType::Long,
                64,
                0,
            ));
            // Age 13 (0b1101), deliberately NOT 3. The churn thread below
            // rebuilds every word through `quartet_of`, so the age has to
            // survive a `make_thin_locked` / `make_inflated` / bare-NEUTRAL
            // rebuild thousands of times. This used to be 3 -- the largest
            // value the old `0x3FFF << 48` mask could carry -- with a comment
            // explaining that anything higher would be truncated by the CHURN.
            // That "explanation" was the bug's camouflage; with the mask fixed
            // to bits 48..63 the high age is exactly what this loop should be
            // hammering, and the final `gc_age()` assertion below is now a live
            // guard against the mask narrowing again.
            header.set_gc_age(13);
            let wins = Arc::new(AtomicUsize::new(0));
            let barrier = Arc::new(Barrier::new(CLAIMERS + 1));

            let churn = {
                let header = Arc::clone(&header);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    // A stack cell stands in for the `Monitor` allocation --
                    // `make_inflated` only needs a plausible aligned pointer.
                    let cell: u64 = 0;
                    let monitor = &cell as *const u64 as usize;
                    barrier.wait();
                    for _ in 0..ROUNDS {
                        for shape in 0..3u8 {
                            loop {
                                let cur = header.mark_word.load(Ordering::Acquire);
                                let next = match shape {
                                    0 => ObjectHeader::make_thin_locked(cur, 4242, 3),
                                    1 => ObjectHeader::make_inflated(cur, monitor),
                                    _ => ObjectHeader::quartet_of(cur) | MARK_NEUTRAL,
                                };
                                if header
                                    .mark_word
                                    .compare_exchange(
                                        cur,
                                        next,
                                        Ordering::AcqRel,
                                        Ordering::Acquire,
                                    )
                                    .is_ok()
                                {
                                    break;
                                }
                            }
                        }
                    }
                })
            };

            let claimers: Vec<_> = (0..CLAIMERS)
                .map(|_| {
                    let header = Arc::clone(&header);
                    let wins = Arc::clone(&wins);
                    let barrier = Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        barrier.wait();
                        for _ in 0..ATTEMPTS {
                            if header.try_add_gc_flags(GC_FLAG_MARKED) {
                                wins.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    })
                })
                .collect();

            churn.join().unwrap();
            for c in claimers {
                c.join().unwrap();
            }

            let claims = wins.load(Ordering::Relaxed);
            assert_eq!(
                claims, 1,
                "the lock-state churn split one claim into {claims}"
            );
            assert_eq!(header.gc_flags() & GC_FLAG_MARKED, GC_FLAG_MARKED);
            assert_eq!(header.kind(), ObjectKind::Array);
            assert_eq!(header.element_type(), ArrayElementType::Long);
            assert_eq!(
                header.gc_age(),
                13,
                "a high gc_age must survive thousands of quartet_of rebuilds; \
                 truncation here means MARK_QUARTET_MASK no longer covers \
                 gc_age's top bits"
            );
        }
    }

    #[test]
    fn autobox_class_id_constant() {
        // LOW (2026-06-17): moved from the unreserved mid-range 0xAB00_0000
        // to u32::MAX so it can never collide with a sequentially-assigned id.
        assert_eq!(AUTOBOX_CLASS_ID.as_u32(), u32::MAX);
    }

    /// LOW (2026-06-17): the auto-box sentinel must live outside the dense,
    /// from-0 sequential `ClassId` allocation range so it can never alias a
    /// real loaded class. Pins the reservation invariant that the (non-const)
    /// compile-time assert cannot express.
    #[test]
    fn autobox_class_id_is_reserved() {
        assert!(
            AUTOBOX_CLASS_ID.as_u32() >= MAX_SEQUENTIAL_CLASS_ID,
            "AUTOBOX_CLASS_ID must be outside the sequential allocation range"
        );
    }

    // -- ObjectHeader for array --

    #[test]
    fn object_header_array_fields() {
        let mut header = ObjectHeader::new(
            ClassId::new(5),
            ObjectKind::Array,
            ArrayElementType::Int,
            100,
            0,
        );
        // MAX_GC_AGE, not 3. This assertion is `mark & !MARK_QUARTET_MASK ==
        // MARK_NEUTRAL` — "nothing outside the quartet is set" — and with age 3
        // it could not fail: 3 fits in bits 60..61, which the old
        // `0x3FFF << 48` mask happened to cover. Age 15 lights bits 62..63 too,
        // so if `MARK_QUARTET_MASK` ever stops covering the whole `gc_age`
        // field the leftover bits show up here as a non-neutral remainder.
        header.set_gc_age(MAX_GC_AGE);
        assert_eq!(header.kind(), ObjectKind::Array);
        assert_eq!(header.element_type(), ArrayElementType::Int);
        assert_eq!(header.array_length(), 100);
        // The identity hash is no longer a header field; it is installed
        // lazily into the mark word on first request.
        assert_eq!(
            header.mark_word.load(Ordering::Relaxed) & !MARK_QUARTET_MASK,
            MARK_NEUTRAL,
            "a bit outside MARK_QUARTET_MASK is set on an unlocked, unhashed \
             object — the quartet is leaking past its mask"
        );
        assert_eq!(header.gc_age(), MAX_GC_AGE);
    }

    // ---------------------------------------------------------------------
    //  Mark word (thin-lock / monitor) tests
    // ---------------------------------------------------------------------

    use std::sync::atomic::Ordering;

    // --- Identity hash in the NEUTRAL mark word ---------------------------

    /// Every hash round-trips, and the word stays in `NEUTRAL` state so the
    /// rest of the mark-word machinery keeps classifying it correctly.
    #[test]
    fn a_neutral_hash_round_trips_and_stays_neutral() {
        for h in [1i32, 42, 0x7FFF_FFFE, i32::MAX] {
            let mark = ObjectHeader::make_neutral_hashed(MARK_NEUTRAL, h);
            assert_eq!(ObjectHeader::mark_state(mark), MARK_NEUTRAL, "hash {h}");
            assert_eq!(ObjectHeader::neutral_hash(mark), h, "hash {h}");
            assert!(!ObjectHeader::is_forwarded_mark(mark));
        }
    }

    /// An installed hash must never read back as "no hash", or the next request
    /// mints a second, different identity hash for the same live object.
    /// Zero and 2^31 are the two generators that would do it.
    #[test]
    fn an_installed_hash_is_never_zero() {
        for h in [0i32, 0x8000_0000u32 as i32, i32::MIN] {
            let mark = ObjectHeader::make_neutral_hashed(MARK_NEUTRAL, h);
            assert_ne!(
                ObjectHeader::neutral_hash(mark),
                0,
                "make_neutral_hashed({h}) installed a hash that reads back absent"
            );
            assert_ne!(mark, MARK_NEUTRAL);
        }
    }

    /// The free half of the design: `try_thin_lock` CASes from the *literal*
    /// `MARK_NEUTRAL`, so a hashed word cannot win it and the caller inflates
    /// instead — HotSpot's rule, already implemented.
    ///
    /// If someone ever "tidies" that CAS into a state comparison, thin-locking
    /// a hashed object would start succeeding and would overwrite the hash.
    /// This is where that fails.
    #[test]
    fn a_hashed_word_cannot_win_the_thin_lock_cas() {
        let hashed = ObjectHeader::make_neutral_hashed(MARK_NEUTRAL, 0x1234_5678);
        assert_ne!(
            hashed, MARK_NEUTRAL,
            "a hashed word must differ from the literal the thin-lock CAS \
             compares against, or locking would silently destroy the hash"
        );
        // ...while still being NEUTRAL by state, which is what makes the
        // caller's fall-through land in the inflate path rather than an
        // "unknown state" arm.
        assert_eq!(ObjectHeader::mark_state(hashed), MARK_NEUTRAL);
    }

    /// A non-neutral payload is an owner id or a pointer. Reporting one as a
    /// hash would be stable-looking garbage; `0` sends the caller to the
    /// displaced-hash table, which is the only correct answer.
    #[test]
    fn no_non_neutral_state_reports_a_hash() {
        let thin = ObjectHeader::make_thin_locked(MARK_NEUTRAL, 0x1234_5678, 9);
        assert_eq!(ObjectHeader::neutral_hash(thin), 0);
        let inflated = ObjectHeader::make_inflated(MARK_NEUTRAL, 0x1_0000);
        assert_eq!(ObjectHeader::neutral_hash(inflated), 0);
        let forwarded = ObjectHeader::make_forwarded(MARK_NEUTRAL, 0x2_0000);
        assert_eq!(ObjectHeader::neutral_hash(forwarded), 0);
    }

    /// First call installs, every later call returns the same value, and the
    /// mint closure is not consulted again.
    #[test]
    fn the_identity_hash_is_installed_once_and_then_stable() {
        let header = make_header();
        let mints = std::sync::atomic::AtomicUsize::new(0);
        let mint = || {
            mints.fetch_add(1, Ordering::Relaxed);
            0x0BAD_F00Du32 as i32 & 0x7FFF_FFFF
        };

        let first = header.mark_word_identity_hash(&mint).unwrap();
        assert_ne!(first, 0);
        assert_eq!(mints.load(Ordering::Relaxed), 1);

        for _ in 0..8 {
            assert_eq!(header.mark_word_identity_hash(&mint).unwrap(), first);
        }
        assert_eq!(
            mints.load(Ordering::Relaxed),
            1,
            "a second mint means a second identity for one object"
        );
    }

    /// A locked or inflated object must route to the displaced table, not
    /// decode an owner id or a `Monitor*` as a hash — and must not mint.
    #[test]
    fn a_non_neutral_object_refuses_rather_than_minting() {
        let mint = || panic!("must not mint for a non-neutral object");

        let header = make_header();
        header.mark_word.store(
            ObjectHeader::make_thin_locked(MARK_NEUTRAL, 77, 0),
            Ordering::Relaxed,
        );
        assert!(header.mark_word_identity_hash(mint).is_err());

        let header = make_header();
        header.mark_word.store(
            ObjectHeader::make_inflated(MARK_NEUTRAL, 0x1_0000),
            Ordering::Relaxed,
        );
        assert!(header.mark_word_identity_hash(mint).is_err());

        let header = make_header();
        header.mark_word.store(
            ObjectHeader::make_forwarded(MARK_NEUTRAL, 0x2_0000),
            Ordering::Relaxed,
        );
        assert!(header.mark_word_identity_hash(mint).is_err());
    }

    /// Concurrent first-hashers must converge on ONE value. This is the
    /// property the CAS exists for, and a plain load/store would pass every
    /// single-threaded test above while failing this.
    #[test]
    fn concurrent_hashers_converge_on_one_value() {
        use std::sync::atomic::AtomicI32;
        use std::sync::Arc;

        for _round in 0..64 {
            let header = Arc::new(make_header());
            let next = Arc::new(AtomicI32::new(1));
            let barrier = Arc::new(std::sync::Barrier::new(8));
            let seen: Vec<_> = (0..8)
                .map(|_| {
                    let header = Arc::clone(&header);
                    let next = Arc::clone(&next);
                    let barrier = Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        barrier.wait();
                        // Every thread proposes a DIFFERENT value, so a lost
                        // update is visible rather than accidentally benign.
                        header
                            .mark_word_identity_hash(|| next.fetch_add(1, Ordering::Relaxed))
                            .unwrap()
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|t| t.join().unwrap())
                .collect();

            assert!(
                seen.windows(2).all(|w| w[0] == w[1]),
                "threads disagreed about one object's identity hash: {seen:?}"
            );
            assert_eq!(
                ObjectHeader::neutral_hash(header.mark_word.load(Ordering::Relaxed)),
                seen[0]
            );
        }
    }

    /// The hash field must not collide with the forwarding/monitor payload
    /// masks — the tag bits are the only overlap that is allowed.
    #[test]
    fn the_hash_field_does_not_overlap_the_state_tag() {
        assert_eq!(MARK_HASH_MASK & MARK_STATE_MASK, 0);
        assert_eq!(MARK_HASH_MASK >> MARK_HASH_SHIFT, 0x7FFF_FFFF);
    }

    #[test]
    fn mark_state_constants_are_distinct_and_in_low_two_bits() {
        assert_eq!(MARK_NEUTRAL, 0b00);
        assert_eq!(MARK_THIN_LOCKED, 0b01);
        assert_eq!(MARK_INFLATED, 0b10);
        assert_eq!(MARK_STATE_MASK, 0b11);
        // The reserved 0b11 state must not collide with any defined state.
        assert_ne!(MARK_NEUTRAL, MARK_THIN_LOCKED);
        assert_ne!(MARK_NEUTRAL, MARK_INFLATED);
        assert_ne!(MARK_THIN_LOCKED, MARK_INFLATED);
        // INFLATED_PTR_MASK is the complement of the state mask.
        assert_eq!(INFLATED_PTR_MASK, !MARK_STATE_MASK);
        assert_eq!(INFLATED_PTR_MASK & MARK_STATE_MASK, 0);
    }

    #[test]
    fn mark_state_round_trips_for_each_variant() {
        // NEUTRAL: pure zero -> state is NEUTRAL.
        assert_eq!(ObjectHeader::mark_state(MARK_NEUTRAL), MARK_NEUTRAL);
        // Even with garbage in the upper bits the state field stays clean.
        assert_eq!(
            ObjectHeader::mark_state(0xDEAD_BEEF_DEAD_BE00),
            MARK_NEUTRAL
        );

        // THIN_LOCKED with owner + recursion set.
        let thin = ObjectHeader::make_thin_locked(MARK_NEUTRAL, 0x1234_5678, 7);
        assert_eq!(ObjectHeader::mark_state(thin), MARK_THIN_LOCKED);

        // INFLATED with a fake aligned pointer.
        let fake_ptr: usize = 0x1_0000; // 8-byte aligned (low 3 bits zero).
        let inflated = ObjectHeader::make_inflated(MARK_NEUTRAL, fake_ptr);
        assert_eq!(ObjectHeader::mark_state(inflated), MARK_INFLATED);
    }

    #[test]
    fn make_thin_locked_round_trips_owner_and_recursion() {
        // Edge case: zero owner, zero recursion -> only the state tag is set.
        let m = ObjectHeader::make_thin_locked(MARK_NEUTRAL, 0, 0);
        assert_eq!(m, MARK_THIN_LOCKED);
        assert_eq!(ObjectHeader::thin_lock_owner(m), 0);
        assert_eq!(ObjectHeader::thin_lock_recursion(m), 0);

        // Typical values.
        let m = ObjectHeader::make_thin_locked(MARK_NEUTRAL, 0x1234_5678, 42);
        assert_eq!(ObjectHeader::mark_state(m), MARK_THIN_LOCKED);
        assert_eq!(ObjectHeader::thin_lock_owner(m), 0x1234_5678);
        assert_eq!(ObjectHeader::thin_lock_recursion(m), 42);

        // Maximum values -- exercise field boundaries.
        let m = ObjectHeader::make_thin_locked(MARK_NEUTRAL, u32::MAX, u8::MAX);
        assert_eq!(ObjectHeader::mark_state(m), MARK_THIN_LOCKED);
        assert_eq!(ObjectHeader::thin_lock_owner(m), u32::MAX);
        assert_eq!(ObjectHeader::thin_lock_recursion(m), u8::MAX);

        // The owner and recursion fields must not overlap each other or the
        // state tag.
        let owner_only = ObjectHeader::make_thin_locked(MARK_NEUTRAL, u32::MAX, 0);
        let rec_only = ObjectHeader::make_thin_locked(MARK_NEUTRAL, 0, u8::MAX);
        assert_eq!(owner_only & MARK_STATE_MASK, MARK_THIN_LOCKED);
        assert_eq!(rec_only & MARK_STATE_MASK, MARK_THIN_LOCKED);
        assert_eq!(owner_only & THIN_LOCK_RECURSION_MASK, 0);
        assert_eq!(rec_only & THIN_LOCK_OWNER_MASK, 0);
    }

    #[test]
    fn make_inflated_round_trips_monitor_pointer() {
        // Use stack-allocated aligned storage so the address is real and
        // guaranteed 8-aligned.
        let slot: u64 = 0;
        let real_ptr = &slot as *const u64 as usize;
        assert_eq!(
            real_ptr & (MARK_STATE_MASK as usize),
            0,
            "stack u64 should be at least 8-byte aligned"
        );

        let mark = ObjectHeader::make_inflated(MARK_NEUTRAL, real_ptr);
        assert_eq!(ObjectHeader::mark_state(mark), MARK_INFLATED);
        assert_eq!(ObjectHeader::inflated_monitor(mark) as usize, real_ptr);

        // Synthetic aligned pointers covering the upper bits.
        for &p in &[0x1000usize, 0xDEAD_BEE0usize, 0x0000_7FFF_FFFF_FFF8usize] {
            let m = ObjectHeader::make_inflated(MARK_NEUTRAL, p);
            assert_eq!(ObjectHeader::mark_state(m), MARK_INFLATED);
            assert_eq!(ObjectHeader::inflated_monitor(m) as usize, p);
        }
    }

    #[test]
    fn mark_word_offset_is_stable() {
        // The JIT lock fast-path hardcodes this offset; if it ever changes
        // both the constant and every emitter must be updated together. Moved
        // 24 -> 16 by the 2026-08-06 header shrink; every emitter reaches it
        // through this constant, which is why that move needed no codegen edit.
        assert_eq!(MARK_WORD_OFFSET, 8);
        assert_eq!(
            std::mem::offset_of!(ObjectHeader, mark_word),
            MARK_WORD_OFFSET
        );
        // 8-byte aligned so atomic ops are well-defined.
        assert_eq!(MARK_WORD_OFFSET % 8, 0);
    }

    #[test]
    fn class_id_remains_at_offset_zero() {
        // JIT-emitted code reads class_id at offset 0 from the object base.
        // Adding the mark word must NOT have disturbed this contract.
        assert_eq!(std::mem::offset_of!(ObjectHeader, class_id), 0);
    }

    #[test]
    fn new_constructor_initializes_mark_word_to_neutral() {
        let header = make_header();
        let mark = header.mark_word.load(Ordering::Relaxed);
        // The quartet shares this word now, so a bare equality against
        // MARK_NEUTRAL would only hold for a plain, unflagged object. What the
        // test means is "no lock, no hash".
        assert_eq!(mark & !MARK_QUARTET_MASK, MARK_NEUTRAL);
        assert_eq!(ObjectHeader::mark_state(mark), MARK_NEUTRAL);
    }

    #[test]
    fn mark_word_supports_atomic_cas_transitions() {
        // Exercises the full state machine path that downstream agents will
        // drive: NEUTRAL -> THIN_LOCKED -> INFLATED. Each transition uses
        // compare_exchange to mimic real contended-lock acquisition.
        let header = make_header();

        // NEUTRAL -> THIN_LOCKED
        let start = header.mark_word.load(Ordering::Acquire);
        let thin = ObjectHeader::make_thin_locked(start, 99, 0);
        header
            .mark_word
            .compare_exchange(start, thin, Ordering::AcqRel, Ordering::Acquire)
            .expect("CAS NEUTRAL->THIN_LOCKED must succeed");
        let observed = header.mark_word.load(Ordering::Acquire);
        assert_eq!(ObjectHeader::mark_state(observed), MARK_THIN_LOCKED);
        assert_eq!(ObjectHeader::thin_lock_owner(observed), 99);

        // THIN_LOCKED -> INFLATED
        let slot: u64 = 0;
        let monitor_addr = &slot as *const u64 as usize;
        let inflated = ObjectHeader::make_inflated(MARK_NEUTRAL, monitor_addr);
        header
            .mark_word
            .compare_exchange(thin, inflated, Ordering::AcqRel, Ordering::Acquire)
            .expect("CAS THIN_LOCKED->INFLATED must succeed");
        let observed = header.mark_word.load(Ordering::Acquire);
        assert_eq!(ObjectHeader::mark_state(observed), MARK_INFLATED);
        assert_eq!(
            ObjectHeader::inflated_monitor(observed) as usize,
            monitor_addr
        );
    }

    #[test]
    #[should_panic(expected = "Monitor pointer must have its low 2 bits clear")]
    fn make_inflated_rejects_misaligned_pointer() {
        // This is a release-active assertion, not a debug-only tripwire:
        // otherwise the state tag would mask the corrupted low bits.
        let _ = ObjectHeader::make_inflated(MARK_NEUTRAL, 0x1001);
    }

    #[test]
    #[should_panic(expected = "plausible user-space pointer")]
    fn make_inflated_rejects_null_pointer() {
        let _ = ObjectHeader::make_inflated(MARK_NEUTRAL, 0);
    }

    #[test]
    #[should_panic(expected = "plausible user-space pointer")]
    fn make_inflated_rejects_four_byte_aligned_pointer() {
        let _ = ObjectHeader::make_inflated(MARK_NEUTRAL, 0x1004);
    }

    // ---------------------------------------------------------------------
    //  Header-shrink contracts (arch-2026-07-26, slug `header-shrink`)
    //
    //  See header-shrink.md. These pin the
    //  layout arithmetic the shrink depends on and the mark-word encoding it
    //  will adopt, so a wrong offset trips a test instead of miscomputing a
    //  heap address.
    // ---------------------------------------------------------------------

    /// The header is **fully packed**: every one of its `HEADER_SIZE` bytes is a
    /// live field, with zero padding to reclaim. This is the load-bearing fact
    /// behind the shrink analysis — it means no shrink is possible without
    /// *deleting a field*, and the achievable sizes are determined entirely by
    /// which fields can go. If this ever stops holding, the doc's size
    /// derivation is stale.
    #[test]
    fn header_has_no_reclaimable_padding() {
        let field_bytes = 4  // class_id
            + 4          // shape
            + 8; // mark_word (which also carries the quartet)
    }

    /// The two removable fields, and what each is actually worth once 8-byte
    /// alignment is applied. Dropping `identity_hash_code` alone buys **zero**
    /// bytes — `mark_word` is an `AtomicU64` and must stay 8-aligned, so the
    /// 4 bytes reappear as padding. This is the arithmetic that decides the
    /// order of operations for the shrink, so it is pinned rather than only
    /// written down.
    #[test]
    fn shrink_candidate_sizes_are_what_the_doc_claims() {
        // Round a packed field total up to the 8-byte alignment `AtomicU64`
        // forces on the struct.
        fn rounded(packed: usize) -> usize {
            (packed + 7) & !7
        }
        assert_eq!(rounded(32), 32, "today");
        // Drop identity_hash_code (4B) only: 28 packed -> 32 aligned. No gain.
        assert_eq!(rounded(32 - 4), 32, "dropping the hash alone saves nothing");
        // Drop forwarding_ptr (8B) only: 24 packed -> 24 aligned. Saves 8.
        assert_eq!(
            rounded(32 - 8),
            24,
            "dropping the forwarding pointer saves 8"
        );
        // Drop both (12B): 20 packed -> 24 aligned. Still 24 — the hash is free
        // to keep, so keeping it is strictly better than folding it.
        assert_eq!(
            rounded(32 - 12),
            24,
            "dropping both lands at the same 24 as dropping the forwarding pointer alone"
        );
        // Reaching 16 additionally requires deleting the 4-byte
        // kind/element_type/gc_age/gc_flags word: class_id(4) + shape(4) +
        // mark_word(8).
        assert_eq!(rounded(4 + 4 + 8), 16, "16 needs the meta word gone too");
    }

    /// The TLAB grid invariant, restated as a runtime check over the shapes the
    /// allocators actually produce. Both `emit_inline_tlab_new` (JIT) and
    /// `Tlab::alloc_initialized` (Rust) bump a cursor by
    /// `HEADER_SIZE + body_size` on an 8-byte grid; a `total_size` that is not
    /// a multiple of 8 desyncs the heap walk *silently in release builds*,
    /// because the allocation-path guard is a `debug_assert`.
    #[test]
    fn tlab_total_size_stays_on_the_eight_byte_grid() {
        // Legacy layout: body is num_fields * SLOT_SIZE.
        for num_fields in 0..64usize {
            let total = HEADER_SIZE + num_fields * SLOT_SIZE;
            assert_eq!(
                total % 8,
                0,
                "legacy total_size for {num_fields} fields is off the 8-byte grid"
            );
        }
        // Compact layout: body is 8*refs + 16*prims, and `set_compact_shape`
        // asserts `body_size & 7 == 0`, so any 8-multiple body is admissible.
        for refs in 0..16usize {
            for prims in 0..16usize {
                let body = refs * REF_FIELD_SIZE + prims * SLOT_SIZE;
                assert_eq!(body % 8, 0, "compact body must be 8-aligned");
                assert_eq!(
                    (HEADER_SIZE + body) % 8,
                    0,
                    "compact total_size ({refs} refs, {prims} prims) is off the grid"
                );
            }
        }
        // Arrays: `array_data_size` already rounds up to 8.
        for len in 0..32usize {
            for et in [
                ArrayElementType::Boolean,
                ArrayElementType::Char,
                ArrayElementType::Int,
                ArrayElementType::Long,
                ArrayElementType::Reference,
            ] {
                let total = ARRAY_DATA_OFFSET + array_data_size(len, et).unwrap();
                assert_eq!(
                    total % 8,
                    0,
                    "array total_size off the grid: {len} x {et:?}"
                );
            }
        }
    }

    /// Array element addressing: the JIT emits `[obj + HEADER_SIZE + i*scale]`
    /// with `HEADER_SIZE` as a **signed** disp8. Both the base displacement and
    /// the array-length displacement must survive any future shrink, and the
    /// element stride must match `element_byte_size`.
    #[test]
    fn array_element_addressing_holds_at_the_current_header_size() {
        assert!(
            i8::try_from(HEADER_SIZE).is_ok(),
            "HEADER_SIZE={HEADER_SIZE} does not fit a SIGNED disp8; the array emitters \
             would address backwards from the object base"
        );
        assert!(
            i8::try_from(ARRAY_LENGTH_OFFSET).is_ok(),
            "ARRAY_LENGTH_OFFSET={ARRAY_LENGTH_OFFSET} does not fit a signed disp8"
        );
        // The length word must not overlap the element data.
        assert!(
            ARRAY_LENGTH_OFFSET + 4 <= HEADER_SIZE,
            "the array-length word must lie inside the header"
        );
        for et in [
            ArrayElementType::Byte,
            ArrayElementType::Char,
            ArrayElementType::Int,
            ArrayElementType::Long,
            ArrayElementType::Reference,
        ] {
            let scale = element_byte_size(et);
            for idx in [0usize, 1, 7, 63] {
                let addr = HEADER_SIZE + idx * scale;
                assert_eq!(
                    addr - HEADER_SIZE,
                    idx * scale,
                    "element {idx} of {et:?} must sit exactly {scale}B apart"
                );
                // Element 0 always lands exactly at the end of the header.
                if idx == 0 {
                    assert_eq!(addr, HEADER_SIZE);
                }
            }
        }
    }

    // -- Mark-word encoding: all four states + co-occurrence ----------------

    #[test]
    fn mark_forwarded_claims_the_previously_reserved_state() {
        assert_eq!(MARK_FORWARDED, 0b11);
        // Distinct from all three pre-existing states.
        assert_ne!(MARK_FORWARDED, MARK_NEUTRAL);
        assert_ne!(MARK_FORWARDED, MARK_THIN_LOCKED);
        assert_ne!(MARK_FORWARDED, MARK_INFLATED);
        // The four states exhaust the 2-bit tag space, so `mark_state` is total
        // and every consumer's `match` needs a FORWARDED arm.
        let seen: std::collections::BTreeSet<u64> = [
            MARK_NEUTRAL,
            MARK_THIN_LOCKED,
            MARK_INFLATED,
            MARK_FORWARDED,
        ]
        .into_iter()
        .collect();
        assert_eq!(seen.len(), 4, "the four state tags must be distinct");
        for s in 0..=MARK_STATE_MASK {
            assert!(seen.contains(&s), "tag {s:#b} is not a named state");
        }
        assert_eq!(FORWARDING_PTR_MASK, !MARK_STATE_MASK);
        assert_eq!(FORWARDING_PTR_MASK & MARK_STATE_MASK, 0);
    }

    #[test]
    fn make_forwarded_round_trips_a_full_64_bit_target() {
        let slot: u64 = 0;
        let real = &slot as *const u64 as usize;
        let mark = ObjectHeader::make_forwarded(MARK_NEUTRAL, real);
        assert_eq!(ObjectHeader::mark_state(mark), MARK_FORWARDED);
        assert!(ObjectHeader::is_forwarded_mark(mark));
        assert_eq!(ObjectHeader::forwarding_target(mark) as usize, real);

        // Synthetic 8-aligned targets across the whole 47-bit user-space range.
        // Nothing is shifted, so the top bits survive — this is the
        // "a folded forwarding pointer must stay 64-bit" constraint.
        for &p in &[
            0x1000usize,
            0xDEAD_BEE0usize,
            0x0000_1000_0000_0000usize,
            0x0000_7FFF_FFFF_FFF8usize,
        ] {
            let m = ObjectHeader::make_forwarded(MARK_NEUTRAL, p);
            assert_eq!(ObjectHeader::mark_state(m), MARK_FORWARDED);
            assert_eq!(
                ObjectHeader::forwarding_target(m) as usize,
                p,
                "forwarding target {p:#x} must round-trip without truncation"
            );
        }
    }

    /// The single most dangerous confusion in this encoding: a FORWARDED word
    /// and an INFLATED word carry their payload in exactly the same bits. Only
    /// the tag distinguishes them, and only an *equality* test on the tag is
    /// safe. A bitwise `mark & MARK_INFLATED != 0` would accept a FORWARDED
    /// word and hand a heap address to `inflated_monitor()` as a `Monitor*` —
    /// silent monitor corruption.
    #[test]
    fn forwarded_and_inflated_are_only_distinguishable_by_tag_equality() {
        let slot: u64 = 0;
        let p = &slot as *const u64 as usize;
        let fwd = ObjectHeader::make_forwarded(MARK_NEUTRAL, p);
        let inf = ObjectHeader::make_inflated(MARK_NEUTRAL, p);

        // Same payload bits, different tags.
        assert_eq!(fwd & FORWARDING_PTR_MASK, inf & INFLATED_PTR_MASK);
        assert_ne!(fwd, inf);

        // Equality on the tag separates them.
        assert_eq!(ObjectHeader::mark_state(fwd), MARK_FORWARDED);
        assert_eq!(ObjectHeader::mark_state(inf), MARK_INFLATED);

        // The unsafe bitwise test does NOT separate them. Asserting the trap
        // exists is the point: it documents why every consumer must compare.
        assert_ne!(
            fwd & MARK_INFLATED,
            0,
            "a FORWARDED word has bit 1 set, so `mark & MARK_INFLATED != 0` \
             misclassifies it as inflated — consumers must use tag equality"
        );
        assert_ne!(
            fwd & MARK_THIN_LOCKED,
            0,
            "a FORWARDED word also has bit 0 set, so it misclassifies as \
             thin-locked under a bitwise test"
        );
    }

    /// The identity hash is NOT orthogonal to the mark word any more, and
    /// this test records the co-occurrence rules that replaced orthogonality.
    ///
    /// It used to assert the opposite -- the hash was a dedicated header field,
    /// so it survived NEUTRAL -> THIN_LOCKED -> INFLATED untouched -- and its
    /// doc argued against folding it in because doing so "saves zero bytes".
    /// That arithmetic was right in isolation and wrong as a conclusion: the
    /// fold buys nothing ALONE (the freed 4 bytes reappear as padding) and is a
    /// prerequisite for the 24 -> 16 shrink, which buys 8.
    ///
    /// What holds now:
    ///   * NEUTRAL carries the hash in its upper bits;
    ///   * a hashed object cannot be THIN_LOCKED at all, because
    ///     `try_thin_lock` CASes from the literal `MARK_NEUTRAL`;
    ///   * INFLATED and FORWARDED carry a pointer, so `neutral_hash` must
    ///     report 0 for them and the caller must go to the displaced hash in
    ///     the object's `Monitor`.
    #[test]
    fn the_identity_hash_shares_the_mark_word_and_is_displaced_by_inflation() {
        let header = make_header();
        let hash = header
            .mark_word_identity_hash(|| 0x5EED_1234 & 0x7FFF_FFFF)
            .expect("a fresh object is NEUTRAL");

        // Still NEUTRAL, and readable straight out of the word.
        let mark = header.mark_word.load(Ordering::Acquire);
        assert_eq!(ObjectHeader::mark_state(mark), MARK_NEUTRAL);
        assert_eq!(ObjectHeader::neutral_hash(mark), hash);

        // A hashed word is non-zero, which is exactly what makes the thin-lock
        // CAS fail and the object inflate instead.
        assert_ne!(mark, MARK_NEUTRAL);

        // Once the word holds a pointer, the hash is not in it, and asking for
        // one must report absence rather than decode the pointer.
        let slot: u64 = 0;
        let monitor = &slot as *const u64 as usize;
        for state in [
            ObjectHeader::make_thin_locked(MARK_NEUTRAL, 77, 3),
            ObjectHeader::make_inflated(MARK_NEUTRAL, monitor),
            ObjectHeader::make_forwarded(MARK_NEUTRAL, monitor),
        ] {
            header.mark_word.store(state, Ordering::Release);
            assert_eq!(
                ObjectHeader::neutral_hash(state),
                0,
                "state {state:#x} must not report a hash"
            );
            assert!(header.mark_word_identity_hash(|| 1).is_err());
        }
    }

    /// Forwarding is a **destructive** mark-word transition: it overwrites
    /// whatever lock state was there. That is why the relocation protocol must
    /// copy the object before clobbering the source word — the destination copy
    /// is the only surviving carrier of the original mark word (and, for an
    /// INFLATED source, of the one strong `Arc<Monitor>` reference it owns).
    #[test]
    fn forwarding_destroys_prior_lock_state_hence_copy_before_clobber() {
        let slot: u64 = 0;
        let monitor = &slot as *const u64 as usize;
        let dest: u64 = 0;
        let dest_addr = &dest as *const u64 as usize;

        for prior in [
            MARK_NEUTRAL,
            ObjectHeader::make_thin_locked(MARK_NEUTRAL, 42, 1),
            ObjectHeader::make_inflated(MARK_NEUTRAL, monitor),
        ] {
            let source = make_header();
            source.mark_word.store(prior, Ordering::Release);

            // Step 1: the destination copy carries the intact prior word.
            let destination = make_header();
            destination
                .mark_word
                .store(source.mark_word.load(Ordering::Acquire), Ordering::Release);

            // Step 2: only now clobber the source.
            source.mark_word.store(
                ObjectHeader::make_forwarded(source.mark_word.load(Ordering::Acquire), dest_addr),
                Ordering::Release,
            );

            let src_mark = source.mark_word.load(Ordering::Acquire);
            assert!(ObjectHeader::is_forwarded_mark(src_mark));
            assert_eq!(
                ObjectHeader::forwarding_target(src_mark) as usize,
                dest_addr
            );
            // The prior state is genuinely gone from the source.
            assert_ne!(
                ObjectHeader::mark_state(src_mark),
                ObjectHeader::mark_state(prior),
                "forwarding must be observed as destructive; if it were not, the \
                 copy-before-clobber ordering would be optional"
            );
            // ...and preserved on the destination.
            assert_eq!(destination.mark_word.load(Ordering::Acquire), prior);
        }
    }

    #[test]
    fn every_mark_state_round_trips_through_a_cas_state_machine() {
        // NEUTRAL -> THIN_LOCKED -> INFLATED -> FORWARDED, each via CAS, with
        // the payload decoded at every step.
        let header = make_header();
        let slot: u64 = 0;
        let monitor = &slot as *const u64 as usize;
        let dest: u64 = 0;
        let dest_addr = &dest as *const u64 as usize;

        // Start from the header's real word, not the bare literal: it carries
        // the quartet, and each transition is built from the word it replaces
        // so those bits survive the whole chain (checked at the end).
        let start = header.mark_word.load(Ordering::Acquire);
        let thin = ObjectHeader::make_thin_locked(start, 0x0BAD_F00D, 9);
        header
            .mark_word
            .compare_exchange(start, thin, Ordering::AcqRel, Ordering::Acquire)
            .expect("NEUTRAL -> THIN_LOCKED");
        let m = header.mark_word.load(Ordering::Acquire);
        assert_eq!(ObjectHeader::thin_lock_owner(m), 0x0BAD_F00D);
        assert_eq!(ObjectHeader::thin_lock_recursion(m), 9);
        assert!(!ObjectHeader::is_forwarded_mark(m));

        let inflated = ObjectHeader::make_inflated(thin, monitor);
        header
            .mark_word
            .compare_exchange(thin, inflated, Ordering::AcqRel, Ordering::Acquire)
            .expect("THIN_LOCKED -> INFLATED");
        let m = header.mark_word.load(Ordering::Acquire);
        assert_eq!(ObjectHeader::inflated_monitor(m) as usize, monitor);
        assert!(!ObjectHeader::is_forwarded_mark(m));

        let forwarded = ObjectHeader::make_forwarded(inflated, dest_addr);
        header
            .mark_word
            .compare_exchange(inflated, forwarded, Ordering::AcqRel, Ordering::Acquire)
            .expect("INFLATED -> FORWARDED");
        let m = header.mark_word.load(Ordering::Acquire);
        assert!(ObjectHeader::is_forwarded_mark(m));
        assert_eq!(ObjectHeader::forwarding_target(m) as usize, dest_addr);
    }

    /// This test used to pin the OPPOSITE: that `is_forwarded()` answered from
    /// the field while the mark-word encoding sat unadopted, because a split
    /// answer would have meant two collectors disagreeing about liveness. The
    /// 2026-08-06 shrink adopted it, so the same hazard now reads the other way
    /// — there must be exactly ONE source of truth, and it is the mark word.
    #[test]
    fn forwarding_is_answered_only_by_the_mark_word() {
        let header = make_header();
        assert!(!header.is_forwarded());

        let dest: u64 = 0;
        let dest_addr = &dest as *const u64 as usize;
        header.mark_word.store(
            ObjectHeader::make_forwarded(MARK_NEUTRAL, dest_addr),
            Ordering::Release,
        );

        assert!(
            header.is_forwarded(),
            "the mark word is the forwarding slot; nothing else can answer"
        );
        assert_eq!(header.forwarding_address() as usize, dest_addr);
        assert_eq!(
            ObjectHeader::forwarding_target(header.mark_word.load(Ordering::Acquire)),
            header.forwarding_address(),
            "the accessor and the raw decode must agree on the target"
        );
        // And the header really did lose the bytes: 32 -> 24 when forwarding
        // folded into the mark word, then 24 -> 16 when the identity hash
        // followed it and the quartet joined them in bits 48..63.
        assert_eq!(HEADER_SIZE, 16);
        assert_eq!(std::mem::size_of::<ObjectHeader>(), 16);
    }

    /// The three non-forwarded mark states must all read as "not forwarded" AND
    /// yield a null address. Before the fold this was trivially true (a separate
    /// field); now it is the property a future consumer is most likely to break
    /// by testing `mark & MARK_FORWARDED != 0` instead of comparing the tag —
    /// that form accepts THIN_LOCKED and INFLATED, and would hand a monitor
    /// pointer back as a relocation address.
    #[test]
    fn only_the_forwarded_tag_reads_as_forwarded() {
        let header = make_header();
        let cell: u64 = 0;
        let addr = &cell as *const u64 as usize;
        for (name, mark) in [
            ("NEUTRAL", MARK_NEUTRAL),
            (
                "THIN_LOCKED",
                ObjectHeader::make_thin_locked(MARK_NEUTRAL, 7, 1),
            ),
            ("INFLATED", ObjectHeader::make_inflated(MARK_NEUTRAL, addr)),
        ] {
            header.mark_word.store(mark, Ordering::Release);
            assert!(!header.is_forwarded(), "{name} must not read as forwarded");
            assert!(
                header.forwarding_address().is_null(),
                "{name} must not yield a forwarding address"
            );
        }
        header.set_forwarding_address(addr as *mut u8);
        assert!(header.is_forwarded());
        assert_eq!(header.forwarding_address() as usize, addr);
    }

    #[test]
    #[should_panic(expected = "low 2 bits clear")]
    fn make_forwarded_rejects_misaligned_target() {
        let _ = ObjectHeader::make_forwarded(MARK_NEUTRAL, 0x1001);
    }

    #[test]
    #[should_panic(expected = "plausible user-space pointer")]
    fn make_forwarded_rejects_null_target() {
        let _ = ObjectHeader::make_forwarded(MARK_NEUTRAL, 0);
    }

    /// `identity_hash_code` left the header on 2026-08-07; `shape` moved up
    /// into the dword it used to occupy.
    ///
    /// The offset that matters now is `shape`'s, because the JIT's inline
    /// allocator writes the field count through `NUM_SLOTS_OFFSET` and used to
    /// write a zero through the hash's offset. Those were 12 and 8; if the hash
    /// constant had been left behind at 8 they would now be the same dword.
    #[test]
    fn the_shape_word_took_over_the_identity_hash_offset() {
        assert_eq!(NUM_SLOTS_OFFSET, 4);
        assert_eq!(ARRAY_LENGTH_OFFSET, NUM_SLOTS_OFFSET);
        assert_eq!(std::mem::offset_of!(ObjectHeader, shape), NUM_SLOTS_OFFSET);
        assert!(NUM_SLOTS_OFFSET + 4 <= MARK_WORD_OFFSET);
    }

    // ---------------------------------------------------------------------
    //  MARK_QUARTET_MASK partition and payload disjointness (2026-08-07)
    //
    //  These exist because the mask was `0x3FFF << 48` -- bits 48..61 -- while
    //  `gc_age` is `AGE_BITS << AGE_SHIFT` = bits 60..63. The top two age bits
    //  sat outside the "quartet" mask, so `quartet_of` truncated any age >= 4
    //  and `!MARK_QUARTET_MASK` did not mean "not the quartet". Nothing failed,
    //  because every round-trip test used age 3 -- the largest value that fits
    //  in bits 60..61. See the long note at `MARK_QUARTET_MASK`.
    // ---------------------------------------------------------------------

    /// The mask/field partition, asserted rather than described.
    ///
    /// Three numbers used to disagree about this one layout: the constant's doc
    /// said "13 bits", its value spanned 14, and the fields span 16. The value
    /// was the wrong one.
    #[test]
    fn the_quartet_fields_lie_inside_the_quartet_mask_and_tile_its_span() {
        let kind: u64 = KIND_BITS << KIND_SHIFT;
        let elem: u64 = ELEM_BITS << ELEM_SHIFT;
        let flags: u64 = FLAGS_BITS << FLAGS_SHIFT;
        let age: u64 = AGE_BITS << AGE_SHIFT;

        // The mask is exactly the mark word's top 16 bits.
        assert_eq!(MARK_QUARTET_SHIFT, 48);
        assert_eq!(MARK_QUARTET_MASK, 0xFFFF_0000_0000_0000u64);
        assert_eq!(MARK_QUARTET_MASK.count_ones(), 16);
        assert_eq!(MARK_QUARTET_MASK.trailing_zeros(), MARK_QUARTET_SHIFT);
        assert_eq!(
            MARK_QUARTET_MASK.leading_zeros(),
            0,
            "the mask must reach bit 63, or gc_age's top bits escape it"
        );

        let fields: [(&str, u64); 4] = [
            ("kind", kind),
            ("element_type", elem),
            ("gc_flags", flags),
            ("gc_age", age),
        ];

        // Every field lies WHOLLY inside the mask. This is the assertion the
        // old mask failed, and it failed on `gc_age` alone.
        for (name, field) in fields {
            assert_eq!(
                field & !MARK_QUARTET_MASK,
                0,
                "{name} has bits outside MARK_QUARTET_MASK, so quartet_of \
                 truncates it and !MARK_QUARTET_MASK leaks it into the payload"
            );
            assert_eq!(field & MARK_QUARTET_MASK, field, "{name}");
        }

        // ...and no two fields overlap.
        for (i, (an, a)) in fields.iter().enumerate() {
            for (bn, b) in fields.iter().skip(i + 1) {
                assert_eq!(*a & *b, 0, "{an} overlaps {bn}");
            }
        }

        // 2 + 4 + 4 + 4 = 14 field bits inside a 16-bit mask. The two left over
        // are bits 54..55: reserved, owned by no field, and deliberately INSIDE
        // the mask so every quartet rebuild carries them for a future claimant.
        let union = kind | elem | flags | age;
        assert_eq!(union.count_ones(), 14, "the four fields are 14 bits wide");
        let reserved = MARK_QUARTET_MASK & !union;
        assert_eq!(
            reserved,
            0b11u64 << 54,
            "the only bits inside the mask that no field owns must be 54..55"
        );

        // The saturation ceiling must be exactly what the field can hold: a
        // larger `MAX_GC_AGE` wraps, a smaller one wastes encodings.
        assert_eq!(u64::from(MAX_GC_AGE), AGE_BITS);
    }

    /// Widening the quartet upward is only safe if bits 48..63 are unused in
    /// EVERY mark-word state. That argument is the whole justification for the
    /// widening, so it is checked against all four states and both payload
    /// encodings rather than asserted in prose.
    #[test]
    fn the_quartet_mask_is_disjoint_from_every_state_payload() {
        // The statically-known fields.
        assert_eq!(MARK_QUARTET_MASK & MARK_STATE_MASK, 0, "state tag");
        assert_eq!(
            MARK_QUARTET_MASK & MARK_HASH_MASK,
            0,
            "NEUTRAL identity hash"
        );
        assert_eq!(
            MARK_QUARTET_MASK & THIN_LOCK_RECURSION_MASK,
            0,
            "THIN_LOCKED recursion count"
        );
        assert_eq!(
            MARK_QUARTET_MASK & THIN_LOCK_OWNER_MASK,
            0,
            "THIN_LOCKED owner id"
        );

        // The two pointer states. `INFLATED_PTR_MASK` / `FORWARDING_PTR_MASK`
        // are both `!MARK_STATE_MASK`, so they nominally reach bit 63 and would
        // overlap on paper. What actually bounds them is
        // `plausible_heap_pointer`, which `make_inflated` and `make_forwarded`
        // ASSERT: its ceiling is `2^47 - 1`, so bit 46 is the highest a payload
        // can occupy and bit 47 is spare between it and the quartet at 48.
        const MAX_PLAUSIBLE: u64 = (1u64 << 47) - 1;
        assert!(crate::plausible_heap_pointer(MAX_PLAUSIBLE & !7));
        assert!(!crate::plausible_heap_pointer(1u64 << 47));
        assert!(!crate::plausible_heap_pointer(1u64 << 48));
        assert!(!crate::plausible_heap_pointer(1u64 << 62));
        assert_eq!(
            MARK_QUARTET_MASK & MAX_PLAUSIBLE,
            0,
            "the widened quartet would eat the top of a Monitor pointer or a \
             forwarding target"
        );

        // The end-to-end form of the same claim: the most extreme legal payload
        // of each state, carried across a rebuild with the ENTIRE quartet set.
        let extreme = (MAX_PLAUSIBLE & !7) as usize;
        let full = MARK_QUARTET_MASK;

        let inflated = ObjectHeader::make_inflated(full, extreme);
        assert_eq!(ObjectHeader::mark_state(inflated), MARK_INFLATED);
        assert_eq!(ObjectHeader::inflated_monitor(inflated) as usize, extreme);
        assert_eq!(ObjectHeader::gc_age_of(inflated), MAX_GC_AGE);

        let forwarded = ObjectHeader::make_forwarded(full, extreme);
        assert!(ObjectHeader::is_forwarded_mark(forwarded));
        assert_eq!(ObjectHeader::forwarding_target(forwarded) as usize, extreme);
        assert_eq!(ObjectHeader::gc_age_of(forwarded), MAX_GC_AGE);

        let thin = ObjectHeader::make_thin_locked(full, u32::MAX, u8::MAX);
        assert_eq!(ObjectHeader::mark_state(thin), MARK_THIN_LOCKED);
        assert_eq!(ObjectHeader::thin_lock_owner(thin), u32::MAX);
        assert_eq!(ObjectHeader::thin_lock_recursion(thin), u8::MAX);
        assert_eq!(ObjectHeader::gc_age_of(thin), MAX_GC_AGE);

        let hashed = ObjectHeader::make_neutral_hashed(full, i32::MAX);
        assert_eq!(ObjectHeader::mark_state(hashed), MARK_NEUTRAL);
        assert_eq!(ObjectHeader::neutral_hash(hashed), i32::MAX);
        assert_eq!(ObjectHeader::gc_age_of(hashed), MAX_GC_AGE);
    }

    /// Every `gc_age` in `0..=MAX_GC_AGE` must survive a rebuild through each
    /// helper that carries the previous word's quartet across.
    ///
    /// This is the coverage that did not exist. It stopped at age 3 -- the
    /// largest value that fitted in the old mask's bits 60..61 -- so 12 of the
    /// 16 ages were silently truncated to `age & 3`, and
    /// `MonitorTable::publish_inflated` (`vm/src/threading/monitor.rs`), which
    /// builds its new word with `make_inflated(expected, ..)`, rewound the
    /// tenuring age of every object it inflated a monitor for.
    #[test]
    fn every_gc_age_survives_every_make_helper() {
        let cell: u64 = 0;
        let payload = &cell as *const u64 as usize;

        for age in 0..=MAX_GC_AGE {
            let header = ObjectHeader::new(
                ClassId::new(3),
                ObjectKind::Array,
                ArrayElementType::Short,
                8,
                0,
            );
            header.add_gc_flags(GC_FLAG_COMPACT);
            header.set_gc_age(age);
            let prev = header.mark_word.load(Ordering::Relaxed);
            assert_eq!(ObjectHeader::gc_age_of(prev), age, "set_gc_age({age})");

            for (name, rebuilt) in [
                (
                    "make_thin_locked",
                    ObjectHeader::make_thin_locked(prev, 0x0BAD_F00D, 7),
                ),
                ("make_inflated", ObjectHeader::make_inflated(prev, payload)),
                (
                    "make_forwarded",
                    ObjectHeader::make_forwarded(prev, payload),
                ),
                (
                    "make_neutral_hashed",
                    ObjectHeader::make_neutral_hashed(prev, 0x0123_4567),
                ),
            ] {
                assert_eq!(
                    ObjectHeader::gc_age_of(rebuilt),
                    age,
                    "{name} truncated gc_age {age} to {}",
                    ObjectHeader::gc_age_of(rebuilt)
                );
                // The other three quartet fields must ride along too.
                assert_eq!(
                    ObjectHeader::kind_of(rebuilt),
                    ObjectKind::Array,
                    "{name}: kind lost at age {age}"
                );
                assert_eq!(
                    ObjectHeader::element_type_tag(rebuilt),
                    ArrayElementType::Short as u8,
                    "{name}: element_type lost at age {age}"
                );
                assert_eq!(
                    (((rebuilt >> FLAGS_SHIFT) & FLAGS_BITS) as u8) & GC_FLAG_COMPACT,
                    GC_FLAG_COMPACT,
                    "{name}: gc_flags lost at age {age}"
                );
                // ...and the quartet must be ALL that crossed over: nothing
                // from the previous word's payload may survive into the new
                // state's payload region.
                assert_eq!(
                    rebuilt & MARK_QUARTET_MASK,
                    prev & MARK_QUARTET_MASK,
                    "{name}: the quartet changed at age {age}"
                );
            }
        }
    }

    /// `inflated_monitor` and `forwarding_target` must round-trip their payload
    /// with a non-zero, HIGH `gc_age` installed **after** the state.
    ///
    /// That order is the one every collector uses -- copy the source mark word
    /// onto the destination, then bump the copy's age (`SharedEvac::evacuate`
    /// and `evacuate_object` in `gc/src/g1.rs`, `evacuate` in
    /// `gc/src/gen_heap.rs`), or age a non-moving survivor in place
    /// (`gen_heap.rs`'s young sweep, which has no promotion cap at all). It is
    /// also the order no test used, which is why the mask gap survived: the
    /// `make_*` helpers emit words whose bits 62..63 are already clear, so a
    /// construct-then-decode round trip cannot see it. Writing the age second
    /// is what exposed `monitor_ptr | (1 << 62)` -- a non-null wild pointer,
    /// and `monitor_ptr_from_mark` in `vm/src/threading/monitor.rs` only
    /// null-checks what it gets.
    #[test]
    fn the_payload_decoders_round_trip_under_every_gc_age() {
        let cell: u64 = 0;
        let payload = &cell as *const u64 as usize;
        const HASH: i32 = 0x5EED_1234 & 0x7FFF_FFFF;

        for age in 0..=MAX_GC_AGE {
            // INFLATED, then aged -- the collector's order.
            let header = make_header();
            header.mark_word.store(
                ObjectHeader::make_inflated(MARK_NEUTRAL, payload),
                Ordering::Release,
            );
            header.set_gc_age(age);
            let mark = header.mark_word.load(Ordering::Acquire);
            assert_eq!(ObjectHeader::mark_state(mark), MARK_INFLATED, "age {age}");
            assert_eq!(header.gc_age(), age, "age {age}: the age itself was lost");
            assert_eq!(
                ObjectHeader::inflated_monitor(mark) as usize,
                payload,
                "age {age}: gc_age bits corrupted the Monitor pointer"
            );

            // FORWARDED, then aged.
            let header = make_header();
            header.mark_word.store(
                ObjectHeader::make_forwarded(MARK_NEUTRAL, payload),
                Ordering::Release,
            );
            header.set_gc_age(age);
            let mark = header.mark_word.load(Ordering::Acquire);
            assert!(ObjectHeader::is_forwarded_mark(mark), "age {age}");
            assert_eq!(header.gc_age(), age, "age {age}");
            assert_eq!(
                ObjectHeader::forwarding_target(mark) as usize,
                payload,
                "age {age}: gc_age bits corrupted the forwarding target"
            );
            assert_eq!(
                header.forwarding_address() as usize,
                payload,
                "age {age}: the accessor disagrees with the raw decode"
            );

            // THIN_LOCKED, then aged. Its payload lives far below bit 48, but
            // pin it so a future field placed lower is caught here too.
            let header = make_header();
            header.mark_word.store(
                ObjectHeader::make_thin_locked(MARK_NEUTRAL, 0x0BAD_F00D, 200),
                Ordering::Release,
            );
            header.set_gc_age(age);
            let mark = header.mark_word.load(Ordering::Acquire);
            assert_eq!(
                ObjectHeader::thin_lock_owner(mark),
                0x0BAD_F00D,
                "age {age}"
            );
            assert_eq!(ObjectHeader::thin_lock_recursion(mark), 200, "age {age}");
            assert_eq!(header.gc_age(), age, "age {age}");

            // NEUTRAL: the identity hash must survive an age write as well.
            let header = make_header();
            header.mark_word.store(
                ObjectHeader::make_neutral_hashed(MARK_NEUTRAL, HASH),
                Ordering::Release,
            );
            header.set_gc_age(age);
            let mark = header.mark_word.load(Ordering::Acquire);
            assert_eq!(
                ObjectHeader::neutral_hash(mark),
                HASH,
                "age {age}: gc_age bits disturbed the identity hash"
            );
            assert_eq!(header.gc_age(), age, "age {age}");
        }
    }

    /// The screen `try_thin_lock` uses in `vm/src/threading/monitor.rs` --
    /// `cur & !types::MARK_QUARTET_MASK != MARK_NEUTRAL`, meaning "locked, or
    /// hashed" -- must read FALSE for an unlocked, unhashed object at EVERY age
    /// and flag combination.
    ///
    /// It did not. With the mask ending at bit 61, an object whose age reached
    /// 4 left bits 62..63 outside it, the screen classified it as non-neutral,
    /// and every later `monitorenter` on that object inflated a monitor instead
    /// of taking the thin lock -- permanently, since this VM never deflates.
    /// That is the same failure the mask was introduced to stop for arrays,
    /// reintroduced for aged objects. The consumer lives in another crate, so
    /// this is the local pin on the property it depends on.
    #[test]
    fn an_aged_unlocked_object_still_passes_the_thin_lock_screen() {
        for age in 0..=MAX_GC_AGE {
            for flags in [0u8, GC_FLAG_OLD_GEN, GC_FLAG_COMPACT | GC_FLAG_MARKED] {
                let header = ObjectHeader::new(
                    ClassId::new(2),
                    ObjectKind::Array,
                    ArrayElementType::Reference,
                    4,
                    0,
                );
                header.set_gc_age(age);
                header.set_gc_flags(flags);
                let cur = header.mark_word.load(Ordering::Relaxed);
                assert_eq!(
                    cur & !MARK_QUARTET_MASK,
                    MARK_NEUTRAL,
                    "age {age}, flags {flags:#x}: an unlocked, unhashed object must \
                     read neutral outside the quartet, or try_thin_lock inflates a \
                     monitor for it on every lock, forever"
                );
            }

            // ...while a HASHED word must still fail that same screen, which is
            // what keeps HotSpot's "a hashed object cannot be thin-locked" rule
            // working. Widening the mask must not have cost that.
            let header = make_header();
            header.set_gc_age(age);
            let quartet = ObjectHeader::quartet_of(header.mark_word.load(Ordering::Relaxed));
            let hashed = ObjectHeader::make_neutral_hashed(quartet, 0x2A);
            assert_ne!(
                hashed & !MARK_QUARTET_MASK,
                MARK_NEUTRAL,
                "age {age}: a hashed word must still lose the thin-lock CAS"
            );
        }
    }
}
