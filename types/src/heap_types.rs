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
pub const HEADER_SIZE: usize = 24;

// JIT x64 emits array element offsets as a *signed* disp8 whose value is
// HEADER_SIZE. If HEADER_SIZE exceeds 127 the disp8 wraps negative and the
// emitted code addresses backwards from the object base. Convert the affected
// emitters to disp32 before allowing HEADER_SIZE to grow beyond this limit;
// the authoritative site inventory is
// `arch-2026-07-26/x64-flag-skew-and-contracts.md` §6.2
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
// state was claimed; see `arch-2026-07-26/header-shrink.md` §4.

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
/// in `arch-2026-07-26/header-shrink.md` §6 are migrated in one
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

/// Byte offset of `mark_word` within `ObjectHeader`. Documented for downstream
/// agents (JIT lock fast-path) so they can emit direct atomic loads / CAS.
///
/// Derived from the `#[repr(C)]` layout: HEADER_SIZE(24) - 8 = 16. The
/// `_const_check_mark_word_offset` assertion below pins this at compile time.
pub const MARK_WORD_OFFSET: usize = 16;

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

/// Byte offset of the array-length/object-shape word.
pub const ARRAY_LENGTH_OFFSET: usize = 12;
pub const NUM_SLOTS_OFFSET: usize = 12;
pub const GC_AGE_OFFSET: usize = 6;
pub const GC_FLAGS_OFFSET: usize = 7;

/// Byte offset of the `identity_hash_code` field within [`ObjectHeader`].
///
/// Added per request R3 of
/// `arch-2026-07-26/x64-flag-skew-and-contracts.md` §7: this was
/// the only header field without a named constant, and it is baked as a literal
/// in at least two places outside `types` (`jit/src/x64.rs` derived its own via
/// `offset_of!` to avoid one; `vm/src/jit/helpers.rs` still writes a bare
/// `raw_ptr.add(8)`). Those should re-export this constant — see
/// `arch-2026-07-26/header-shrink.md` §6.
pub const IDENTITY_HASH_CODE_OFFSET: usize = 8;

// Compile-time check that the offset is correct.
const _: () = assert!(
    std::mem::offset_of!(ObjectHeader, shape) == ARRAY_LENGTH_OFFSET,
    "ARRAY_LENGTH_OFFSET must match ObjectHeader layout"
);

const _: () = assert!(
    std::mem::offset_of!(ObjectHeader, identity_hash_code) == IDENTITY_HASH_CODE_OFFSET,
    "IDENTITY_HASH_CODE_OFFSET must match ObjectHeader layout"
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
/// Layout (32 bytes total, 8-byte aligned):
/// - `class_id`: ClassId (4 bytes) -- MUST stay at offset 0 (JIT contract)
/// - `kind`: ObjectKind (1 byte)
/// - `element_type`: ArrayElementType (1 byte, only meaningful for arrays)
/// - `gc_age`: u8
/// - `gc_flags`: u8
/// - `identity_hash_code`: i32 (4 bytes)
/// - `shape`: u32 (array length, or full instance-field count)
/// - `forwarding_ptr`: *mut u8 (8 bytes, used by GC for object relocation)
/// - `mark_word`: AtomicU64 (8 bytes, thin-lock / monitor state -- offset 24)
///
/// NOTE: `Clone`/`Copy` were removed when `mark_word: AtomicU64` was added,
/// since atomics are `!Copy`. Header copies must now go through explicit
/// field-by-field reconstruction (or `std::ptr::copy_nonoverlapping` at the
/// raw byte level during GC relocation).
#[repr(C)]
#[derive(Debug)]
pub struct ObjectHeader {
    pub class_id: ClassId,
    pub kind: ObjectKind,
    pub element_type: ArrayElementType,
    /// GC age -- number of times this object survived a minor GC (0..15).
    pub gc_age: u8,
    /// GC flags -- old/marked/compact layout bits.
    pub gc_flags: u8,
    pub identity_hash_code: i32,
    /// Arrays store their length directly. Objects store the full 32-bit
    /// hierarchy-wide instance-field count.
    pub shape: u32,
    /// Mark word -- thin-lock owner / recursion / inflated-monitor pointer /
    /// **GC forwarding target**. State encoded in low 2 bits; see
    /// `MARK_NEUTRAL` / `MARK_THIN_LOCKED` / `MARK_INFLATED` /
    /// `MARK_FORWARDED`. Always at byte offset `MARK_WORD_OFFSET` (= 16).
    /// Initialized to `MARK_NEUTRAL` by `ObjectHeader::new`.
    ///
    /// The dedicated `forwarding_ptr` field that used to sit at offset 16 was
    /// deleted on 2026-08-06: its state had a designed, tested and unused
    /// encoding here since 2026-07-26, and it was the ONLY reclaimable 8 bytes
    /// in the header (folding `identity_hash_code` instead buys zero, because
    /// `AtomicU64` forces 8-byte alignment and the 4 bytes reappear as
    /// padding). See `docs/internal/arch-2026-07-26/header-shrink.md`.
    pub mark_word: AtomicU64,
}

/// Byte offset of the `kind` field within `ObjectHeader`.
pub const OBJECT_KIND_OFFSET: usize = 4;

/// Byte offset of the `element_type` field within `ObjectHeader`.
pub const ARRAY_ELEMENT_TYPE_OFFSET: usize = 5;

const _: () = assert!(
    std::mem::offset_of!(ObjectHeader, kind) == OBJECT_KIND_OFFSET,
    "OBJECT_KIND_OFFSET must match ObjectHeader layout"
);

const _: () = assert!(
    std::mem::offset_of!(ObjectHeader, element_type) == ARRAY_ELEMENT_TYPE_OFFSET,
    "ARRAY_ELEMENT_TYPE_OFFSET must match ObjectHeader layout"
);

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
        identity_hash_code: i32,
        array_length: u32,
        num_slots: u32,
    ) -> Self {
        let shape = if kind == ObjectKind::Array {
            array_length
        } else {
            num_slots
        };
        Self {
            class_id,
            kind,
            element_type,
            gc_age: 0,
            gc_flags: 0,
            identity_hash_code,
            shape,
            mark_word: AtomicU64::new(MARK_NEUTRAL),
        }
    }

    #[inline]
    pub fn array_length(&self) -> u32 {
        if self.kind == ObjectKind::Array {
            self.shape
        } else {
            0
        }
    }

    #[inline]
    pub fn set_array_length(&mut self, length: u32) {
        debug_assert_eq!(self.kind, ObjectKind::Array);
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
        self.gc_flags |= GC_FLAG_COMPACT;
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
    /// `docs/internal/arch-2026-07-26/header-shrink.md` §4.3.
    pub fn set_forwarding_address(&self, target: *mut u8) {
        self.mark_word.store(
            Self::make_forwarded(target as usize),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Returns true if this object is in the old generation.
    pub fn is_old_gen(&self) -> bool {
        self.gc_flags & GC_FLAG_OLD_GEN != 0
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
    pub fn make_thin_locked(thread_id: u32, recursion: u8) -> u64 {
        MARK_THIN_LOCKED
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
    pub fn make_inflated(monitor_ptr: usize) -> u64 {
        assert!(
            monitor_ptr & (MARK_STATE_MASK as usize) == 0,
            "Monitor pointer must have its low 2 bits clear (>= 4-byte aligned)"
        );
        assert!(
            crate::plausible_heap_pointer(monitor_ptr as u64),
            "Monitor pointer must be a non-null, 8-byte aligned plausible user-space pointer"
        );
        (monitor_ptr as u64) | MARK_INFLATED
    }

    /// Decode the `Monitor` pointer from a `MARK_INFLATED` mark word.
    /// Result is meaningless if the mark word is not in inflated state.
    #[inline(always)]
    pub fn inflated_monitor(mark: u64) -> *mut () {
        ((mark & INFLATED_PTR_MASK) as usize) as *mut ()
    }

    /// Construct a `MARK_FORWARDED` mark word pointing at an object's new
    /// location after GC relocation.
    ///
    /// The target keeps its **full 64-bit** width: the address is OR-ed with
    /// the tag rather than shifted, so no high bits are lost and no overflow
    /// side table is required. Heap objects are 8-byte aligned, so the low 3
    /// bits are already zero and borrowing 2 of them is free.
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
    /// `arch-2026-07-26/header-shrink.md` §4.
    #[inline(always)]
    pub fn make_forwarded(target: usize) -> u64 {
        assert!(
            target & (MARK_STATE_MASK as usize) == 0,
            "forwarding target must have its low 2 bits clear (>= 4-byte aligned)"
        );
        assert!(
            crate::plausible_heap_pointer(target as u64),
            "forwarding target must be a non-null, 8-byte aligned plausible user-space pointer"
        );
        (target as u64) | MARK_FORWARDED
    }

    /// Decode the relocation target from a `MARK_FORWARDED` mark word.
    /// Result is meaningless if the mark word is not in forwarded state;
    /// check with [`ObjectHeader::is_forwarded_mark`] first.
    #[inline(always)]
    pub fn forwarding_target(mark: u64) -> *mut u8 {
        ((mark & FORWARDING_PTR_MASK) as usize) as *mut u8
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

    // Helper to create a default ObjectHeader for testing.
    fn make_header() -> ObjectHeader {
        ObjectHeader::new(
            ClassId::new(1),
            ObjectKind::Object,
            ArrayElementType::Boolean,
            0,
            0,
            0,
        )
    }

    // -- Constants --

    #[test]
    fn header_size_is_correct() {
        assert_eq!(HEADER_SIZE, 24);
        assert_eq!(std::mem::size_of::<ObjectHeader>(), HEADER_SIZE);
        assert_eq!(std::mem::align_of::<ObjectHeader>(), 8);
        assert_eq!(std::mem::offset_of!(ObjectHeader, class_id), 0);
        assert_eq!(std::mem::offset_of!(ObjectHeader, kind), OBJECT_KIND_OFFSET);
        assert_eq!(
            std::mem::offset_of!(ObjectHeader, element_type),
            ARRAY_ELEMENT_TYPE_OFFSET
        );
        assert_eq!(std::mem::offset_of!(ObjectHeader, gc_age), GC_AGE_OFFSET);
        assert_eq!(std::mem::offset_of!(ObjectHeader, gc_flags), GC_FLAGS_OFFSET);
        assert_eq!(std::mem::offset_of!(ObjectHeader, identity_hash_code), 8);
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
        assert_eq!(header.gc_flags & GC_FLAG_COMPACT, GC_FLAG_COMPACT);
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
        header.gc_flags = GC_FLAG_OLD_GEN;
        assert!(header.is_old_gen());
    }

    #[test]
    fn object_header_marked_flag_does_not_imply_old_gen() {
        let mut header = make_header();
        header.gc_flags = GC_FLAG_MARKED;
        assert!(!header.is_old_gen());
    }

    #[test]
    fn object_header_combined_flags() {
        let mut header = make_header();
        header.gc_flags = GC_FLAG_OLD_GEN | GC_FLAG_MARKED;
        assert!(header.is_old_gen());
    }

    #[test]
    fn gc_flag_constants() {
        assert_eq!(GC_FLAG_OLD_GEN, 0x01);
        assert_eq!(GC_FLAG_MARKED, 0x02);
        // Flags should not overlap
        assert_eq!(GC_FLAG_OLD_GEN & GC_FLAG_MARKED, 0);
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
            12345,
            100,
            0,
        );
        header.gc_age = 3;
        assert_eq!(header.kind, ObjectKind::Array);
        assert_eq!(header.element_type, ArrayElementType::Int);
        assert_eq!(header.array_length(), 100);
        assert_eq!(header.identity_hash_code, 12345);
        assert_eq!(header.gc_age, 3);
    }

    // ---------------------------------------------------------------------
    //  Mark word (thin-lock / monitor) tests
    // ---------------------------------------------------------------------

    use std::sync::atomic::Ordering;

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
        let thin = ObjectHeader::make_thin_locked(0x1234_5678, 7);
        assert_eq!(ObjectHeader::mark_state(thin), MARK_THIN_LOCKED);

        // INFLATED with a fake aligned pointer.
        let fake_ptr: usize = 0x1_0000; // 8-byte aligned (low 3 bits zero).
        let inflated = ObjectHeader::make_inflated(fake_ptr);
        assert_eq!(ObjectHeader::mark_state(inflated), MARK_INFLATED);
    }

    #[test]
    fn make_thin_locked_round_trips_owner_and_recursion() {
        // Edge case: zero owner, zero recursion -> only the state tag is set.
        let m = ObjectHeader::make_thin_locked(0, 0);
        assert_eq!(m, MARK_THIN_LOCKED);
        assert_eq!(ObjectHeader::thin_lock_owner(m), 0);
        assert_eq!(ObjectHeader::thin_lock_recursion(m), 0);

        // Typical values.
        let m = ObjectHeader::make_thin_locked(0x1234_5678, 42);
        assert_eq!(ObjectHeader::mark_state(m), MARK_THIN_LOCKED);
        assert_eq!(ObjectHeader::thin_lock_owner(m), 0x1234_5678);
        assert_eq!(ObjectHeader::thin_lock_recursion(m), 42);

        // Maximum values -- exercise field boundaries.
        let m = ObjectHeader::make_thin_locked(u32::MAX, u8::MAX);
        assert_eq!(ObjectHeader::mark_state(m), MARK_THIN_LOCKED);
        assert_eq!(ObjectHeader::thin_lock_owner(m), u32::MAX);
        assert_eq!(ObjectHeader::thin_lock_recursion(m), u8::MAX);

        // The owner and recursion fields must not overlap each other or the
        // state tag.
        let owner_only = ObjectHeader::make_thin_locked(u32::MAX, 0);
        let rec_only = ObjectHeader::make_thin_locked(0, u8::MAX);
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

        let mark = ObjectHeader::make_inflated(real_ptr);
        assert_eq!(ObjectHeader::mark_state(mark), MARK_INFLATED);
        assert_eq!(ObjectHeader::inflated_monitor(mark) as usize, real_ptr);

        // Synthetic aligned pointers covering the upper bits.
        for &p in &[0x1000usize, 0xDEAD_BEE0usize, 0x0000_7FFF_FFFF_FFF8usize] {
            let m = ObjectHeader::make_inflated(p);
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
        assert_eq!(MARK_WORD_OFFSET, 16);
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
        assert_eq!(mark, MARK_NEUTRAL);
        assert_eq!(ObjectHeader::mark_state(mark), MARK_NEUTRAL);
    }

    #[test]
    fn mark_word_supports_atomic_cas_transitions() {
        // Exercises the full state machine path that downstream agents will
        // drive: NEUTRAL -> THIN_LOCKED -> INFLATED. Each transition uses
        // compare_exchange to mimic real contended-lock acquisition.
        let header = make_header();

        // NEUTRAL -> THIN_LOCKED
        let thin = ObjectHeader::make_thin_locked(99, 0);
        header
            .mark_word
            .compare_exchange(MARK_NEUTRAL, thin, Ordering::AcqRel, Ordering::Acquire)
            .expect("CAS NEUTRAL->THIN_LOCKED must succeed");
        let observed = header.mark_word.load(Ordering::Acquire);
        assert_eq!(ObjectHeader::mark_state(observed), MARK_THIN_LOCKED);
        assert_eq!(ObjectHeader::thin_lock_owner(observed), 99);

        // THIN_LOCKED -> INFLATED
        let slot: u64 = 0;
        let monitor_addr = &slot as *const u64 as usize;
        let inflated = ObjectHeader::make_inflated(monitor_addr);
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
        let _ = ObjectHeader::make_inflated(0x1001);
    }

    #[test]
    #[should_panic(expected = "plausible user-space pointer")]
    fn make_inflated_rejects_null_pointer() {
        let _ = ObjectHeader::make_inflated(0);
    }

    #[test]
    #[should_panic(expected = "plausible user-space pointer")]
    fn make_inflated_rejects_four_byte_aligned_pointer() {
        let _ = ObjectHeader::make_inflated(0x1004);
    }

    // ---------------------------------------------------------------------
    //  Header-shrink contracts (arch-2026-07-26, slug `header-shrink`)
    //
    //  See arch-2026-07-26/header-shrink.md. These pin the
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
            + 1              // kind
            + 1              // element_type
            + 1              // gc_age
            + 1              // gc_flags
            + 4              // identity_hash_code
            + 4              // shape
            + 8; // mark_word (also the GC forwarding slot since 2026-08-06)
        assert_eq!(
            field_bytes, HEADER_SIZE,
            "ObjectHeader is fully packed; a shrink must delete a field, not padding"
        );
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
                let total = HEADER_SIZE + array_data_size(len, et).unwrap();
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
        let mark = ObjectHeader::make_forwarded(real);
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
            let m = ObjectHeader::make_forwarded(p);
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
        let fwd = ObjectHeader::make_forwarded(p);
        let inf = ObjectHeader::make_inflated(p);

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

    /// `identity_hash_code` is a **dedicated header field**, not a mark-word
    /// resident, so it is orthogonal to every lock transition. This is the
    /// real co-occurrence matrix for this VM: a hash installed once survives
    /// NEUTRAL -> THIN_LOCKED -> INFLATED unchanged. (There is deliberately no
    /// "hashed" mark-word state: folding the hash into the mark word saves
    /// zero bytes — see `shrink_candidate_sizes_are_what_the_doc_claims` — so
    /// inventing one would add a collision surface for no gain.)
    #[test]
    fn identity_hash_is_orthogonal_to_every_mark_word_state() {
        let mut header = make_header();
        header.identity_hash_code = 0x5EED_1234u32 as i32;

        let slot: u64 = 0;
        let monitor = &slot as *const u64 as usize;
        let thin = ObjectHeader::make_thin_locked(77, 3);
        let inflated = ObjectHeader::make_inflated(monitor);

        for state in [MARK_NEUTRAL, thin, inflated] {
            header.mark_word.store(state, Ordering::Release);
            assert_eq!(
                header.identity_hash_code, 0x5EED_1234u32 as i32,
                "the identity hash must be unaffected by mark-word state {state:#x}"
            );
        }
        // ...and the mark word is likewise unaffected by rewriting the hash.
        header.mark_word.store(inflated, Ordering::Release);
        header.identity_hash_code = -1;
        assert_eq!(header.mark_word.load(Ordering::Acquire), inflated);
        assert_eq!(
            ObjectHeader::inflated_monitor(header.mark_word.load(Ordering::Acquire)) as usize,
            monitor
        );
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
            ObjectHeader::make_thin_locked(42, 1),
            ObjectHeader::make_inflated(monitor),
        ] {
            let source = make_header();
            source.mark_word.store(prior, Ordering::Release);

            // Step 1: the destination copy carries the intact prior word.
            let destination = make_header();
            destination
                .mark_word
                .store(source.mark_word.load(Ordering::Acquire), Ordering::Release);

            // Step 2: only now clobber the source.
            source
                .mark_word
                .store(ObjectHeader::make_forwarded(dest_addr), Ordering::Release);

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

        let thin = ObjectHeader::make_thin_locked(0x0BAD_F00D, 9);
        header
            .mark_word
            .compare_exchange(MARK_NEUTRAL, thin, Ordering::AcqRel, Ordering::Acquire)
            .expect("NEUTRAL -> THIN_LOCKED");
        let m = header.mark_word.load(Ordering::Acquire);
        assert_eq!(ObjectHeader::thin_lock_owner(m), 0x0BAD_F00D);
        assert_eq!(ObjectHeader::thin_lock_recursion(m), 9);
        assert!(!ObjectHeader::is_forwarded_mark(m));

        let inflated = ObjectHeader::make_inflated(monitor);
        header
            .mark_word
            .compare_exchange(thin, inflated, Ordering::AcqRel, Ordering::Acquire)
            .expect("THIN_LOCKED -> INFLATED");
        let m = header.mark_word.load(Ordering::Acquire);
        assert_eq!(ObjectHeader::inflated_monitor(m) as usize, monitor);
        assert!(!ObjectHeader::is_forwarded_mark(m));

        let forwarded = ObjectHeader::make_forwarded(dest_addr);
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
        header
            .mark_word
            .store(ObjectHeader::make_forwarded(dest_addr), Ordering::Release);

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
        // And the header really did lose the eight bytes.
        assert_eq!(HEADER_SIZE, 24);
        assert_eq!(std::mem::size_of::<ObjectHeader>(), 24);
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
            ("THIN_LOCKED", ObjectHeader::make_thin_locked(7, 1)),
            ("INFLATED", ObjectHeader::make_inflated(addr)),
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
        let _ = ObjectHeader::make_forwarded(0x1001);
    }

    #[test]
    #[should_panic(expected = "plausible user-space pointer")]
    fn make_forwarded_rejects_null_target() {
        let _ = ObjectHeader::make_forwarded(0);
    }

    #[test]
    fn identity_hash_code_offset_is_named_and_pinned() {
        assert_eq!(IDENTITY_HASH_CODE_OFFSET, 8);
        assert_eq!(
            std::mem::offset_of!(ObjectHeader, identity_hash_code),
            IDENTITY_HASH_CODE_OFFSET
        );
        // It must not overlap the neighbouring fields.
        assert!(GC_FLAGS_OFFSET < IDENTITY_HASH_CODE_OFFSET);
        assert!(IDENTITY_HASH_CODE_OFFSET + 4 <= NUM_SLOTS_OFFSET);
    }
}
