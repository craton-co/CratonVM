// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Heap object layout types and constants.
//!
//! Defines the on-heap object header layout shared by the GC, JIT, and
//! interpreter and the related compile-time layout invariants.
//!
//! # Two header lengths (2026-09-24)
//!
//! Every heap object starts with one 8-byte **header word**: the `class_id`
//! (offset 0, a plain `u32` the JIT compares directly) and the 32-bit **mark
//! word** (offset 4: lock state, identity hash, forwarding state and the
//! kind / element type / GC flags / GC age quartet).
//!
//! * A **compact** instance (`GC_FLAG_COMPACT`, laid out by a registered
//!   [`crate::field_layout::CompactLayout`]) stops there: its fields start at
//!   [`COMPACT_HEADER_SIZE`] (8). Its field count is a property of its class,
//!   so it needs no shape word. This is what makes a two-reference tree node
//!   24 bytes instead of 32.
//! * **Arrays** and **legacy** (16-byte `Value`-cell) instances carry a second
//!   word: the `shape` (array length or instance-field count) at offset 8 and
//!   the identity hash at offset 12. Their payload starts at [`HEADER_SIZE`]
//!   (16), exactly where it always did.
//!
//! `HEADER_SIZE` therefore keeps its old meaning — the long header, and the
//! start of legacy cells — and the compact field base is a separate constant.
//! The layout registry hands out *absolute* field displacements (already
//! including [`COMPACT_HEADER_SIZE`]), so no consumer adds a header size to a
//! compact offset any more.
//!
//! Every object is at least [`MIN_OBJECT_SIZE`] (16) bytes, so every object has
//! a second word. A relocating collector stores the forwarding target there
//! (see [`ObjectHeader::set_forwarding_address`]) and leaves the header word —
//! class id and quartet — intact in the from-space copy.

use crate::ClassId;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Size of the **long** object header in bytes: the header word plus the
/// shape/aux word. Arrays and legacy (non-compact) instances use it; their
/// elements / 16-byte field cells start here.
///
/// A compact instance's header is [`COMPACT_HEADER_SIZE`]; its fields are
/// addressed through the absolute displacements of its
/// [`crate::field_layout::CompactLayout`], never through this constant.
pub const HEADER_SIZE: usize = 16;

/// Size of a **compact** instance's header: the header word alone. Its fields
/// start here.
pub const COMPACT_HEADER_SIZE: usize = 8;

/// Size of the header word every object starts with (`class_id` + mark word).
pub const HEADER_WORD_SIZE: usize = 8;

/// Smallest object any allocator hands out. Every object has a second word,
/// which is where a relocating collector parks the forwarding target (see
/// [`FORWARDING_TARGET_OFFSET`]). A compact layout's `total_size` is padded up
/// to it.
pub const MIN_OBJECT_SIZE: usize = 16;

/// Byte offset of the forwarding target in a `FORWARDED` object: the second
/// word, which is body for a compact instance and shape/aux for a long one.
pub const FORWARDING_TARGET_OFFSET: usize = 8;

const _: () = assert!(COMPACT_HEADER_SIZE == HEADER_WORD_SIZE);
const _: () = assert!(MIN_OBJECT_SIZE >= FORWARDING_TARGET_OFFSET + 8);
const _: () = assert!(MIN_OBJECT_SIZE <= HEADER_SIZE);

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

// --- Mark word (lock / hash / forwarding state) ------------------------------
//
// The mark word is the `AtomicU32` at offset 4 of every object. Low 2 bits:
//
//   00 = NEUTRAL      (no lock held; a compact instance may carry its hash)
//   01 = THIN_LOCKED  (owner lock-slot + recursion count in bits 2..15)
//   10 = INFLATED     (a `Monitor` exists; found by object address in the
//                      VM's monitor index, which every moving collector remaps)
//   11 = FORWARDED    (relocated; the target is in the object's second word,
//                      see `FORWARDING_TARGET_OFFSET`)
//
// Bits 2..15 are the state payload; bits 16..31 are the quartet
// (`MARK_QUARTET_MASK`). All transitions are CASes on this word and every one
// is built from the previous value, so the quartet survives them.
//
// The states are mutually exclusive and compared for *equality* (never tested
// with a bitwise AND) -- `mark & MARK_INFLATED != 0` would alias FORWARDED.

/// Mark word state: no lock held.
pub const MARK_NEUTRAL: u32 = 0b00;
/// Mark word state: object is thin-locked by a single thread.
pub const MARK_THIN_LOCKED: u32 = 0b01;
/// Mark word state: object's monitor has been inflated to a heap `Monitor`,
/// which is found by the object's address.
pub const MARK_INFLATED: u32 = 0b10;
/// Mark word state: object has been relocated by the GC; the target is the
/// object's second word (unless [`MARK_FWD_SELF`] is set).
pub const MARK_FORWARDED: u32 = 0b11;
/// Mask covering the 2-bit state field of the mark word.
pub const MARK_STATE_MASK: u32 = 0b11;

/// First bit of the state payload.
pub const MARK_PAYLOAD_SHIFT: u32 = 2;

/// Rounds [`ObjectHeader::forwarding_address`] probes a [`MARK_FWD_BUSY`]
/// claim before answering "not forwarded". A real claim publishes two stores
/// later; a stale or header-shaped non-object word must not turn a conservative
/// screen into an unbounded scheduler-yield loop.
pub const FORWARDING_BUSY_WAIT_LIMIT: u32 = 1 << 12;

/// How often a BUSY wait hit [`FORWARDING_BUSY_WAIT_LIMIT`]. Nonzero means a
/// header screen met a word that looked like a claim in progress.
pub static FORWARDING_BUSY_WAIT_ABANDONED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// How often [`ObjectHeader::screen_shape`] refused a candidate whose mark
/// stayed `FORWARDED | BUSY` (gen r5w6/pin10). Read through
/// [`ObjectHeader::screen_busy_declined`].
static SCREEN_BUSY_DECLINED: AtomicU64 = AtomicU64::new(0);
/// The 14-bit state payload (bits 2..15).
pub const MARK_PAYLOAD_MASK: u32 = 0x3FFF << MARK_PAYLOAD_SHIFT;

/// Thin-lock mark word layout:
///   bits 0-1:   state = `MARK_THIN_LOCKED`
///   bits 2-12:  owner lock slot (11 bits; see `MAX_THIN_LOCK_SLOT`)
///   bits 13-15: recursion count (0 = held once; max 7 -> 8 nested)
///
/// The owner is a small **lock slot** the VM leases to a thread, not its
/// `ThreadId`: thread ids are a never-recycled `u64` counter and do not fit.
/// A thread without a slot, or a recursion past the limit, inflates.
pub const THIN_LOCK_OWNER_SHIFT: u32 = 2;
pub const THIN_LOCK_OWNER_MASK: u32 = 0x7FF << THIN_LOCK_OWNER_SHIFT;
pub const THIN_LOCK_RECURSION_SHIFT: u32 = 13;
pub const THIN_LOCK_RECURSION_MASK: u32 = 0x7 << THIN_LOCK_RECURSION_SHIFT;
/// Largest lock slot a thin-lock word can name.
pub const MAX_THIN_LOCK_SLOT: u32 = 0x7FF;
/// Largest recursion count a thin-lock word can hold (held `MAX + 1` times).
pub const MAX_THIN_LOCK_RECURSION: u32 = 0x7;

/// Forwarded payload bit: the object was forwarded to **itself** (a G1
/// evacuation failure, or a sliding compactor's object that does not move).
/// The second word is NOT written -- the body stays intact -- and the target
/// is the object's own address.
pub const MARK_FWD_SELF: u32 = 1 << 2;
/// Forwarded payload bit: a parallel evacuator has claimed the object and is
/// still writing the target into the second word. Readers spin until it
/// clears (see [`ObjectHeader::forwarding_address`]).
pub const MARK_FWD_BUSY: u32 = 1 << 3;

// --- Identity hash ------------------------------------------------------------
//
// * A **long-header** object (array, legacy instance) keeps its identity hash
//   in the aux word at `IDENTITY_HASH_OFFSET` (offset 12): 31 bits, installed
//   lazily by one CAS from 0, independent of lock state -- a hashed array can
//   still thin-lock.
// * A **compact** instance has no aux word. Its hash lives in a NEUTRAL mark
//   word: 14 bits in the payload plus the 6 bits a compact instance does not
//   need for `element_type` / reserved (`MARK_SHORT_HASH_HI_MASK`), 20 bits
//   in all, mixed with the class id into a 31-bit value on the way out. It is
//   HotSpot's rule that a hashed object cannot thin-lock: `try_thin_lock`
//   refuses a hashed word and the caller inflates, displacing the hash into
//   the `Monitor`.

/// Low part of a compact instance's identity hash: the whole NEUTRAL payload.
pub const MARK_HASH_SHIFT: u32 = MARK_PAYLOAD_SHIFT;
pub const MARK_HASH_MASK: u32 = MARK_PAYLOAD_MASK;
/// High part of a compact instance's identity hash: the `element_type` and
/// reserved bits (18..23), which a plain object never uses for anything else.
pub const MARK_SHORT_HASH_HI_SHIFT: u32 = 18;
pub const MARK_SHORT_HASH_HI_MASK: u32 = 0x3F << MARK_SHORT_HASH_HI_SHIFT;
/// Width of a compact instance's in-header hash.
pub const SHORT_HASH_BITS: u32 = 20;

/// Byte offset of the mark word within every object.
pub const MARK_WORD_OFFSET: usize = 4;

/// Byte offset of a long-header object's identity hash (the aux word).
pub const IDENTITY_HASH_OFFSET: usize = 12;

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

/// Byte offset of an array's **data area** from the object base: past the
/// header word and the length/aux word. Equal to [`HEADER_SIZE`] (the long
/// header), and a separate name so that array sites say what they mean.
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

/// Byte offset of an array's length (and a legacy instance's field count): the
/// `shape` word, the first half of a long header's second word.
///
/// A compact instance has no shape word -- offset 8 is its first field.
pub const ARRAY_LENGTH_OFFSET: usize = 8;

// Emitted as a signed disp8 against the object base at every JIT array-length
// site, so it must fit -128..=127 or those instructions silently address
// BEFORE the object.
const _: () = assert!(
    ARRAY_LENGTH_OFFSET <= 127,
    "ARRAY_LENGTH_OFFSET must fit a signed disp8 for JIT array-length loads"
);
const _: () = assert!(ARRAY_LENGTH_OFFSET + 4 <= ARRAY_DATA_OFFSET);

/// Byte offset of a **legacy** instance's field count. Same word as
/// [`ARRAY_LENGTH_OFFSET`]. Never read it from a compact instance.
pub const NUM_SLOTS_OFFSET: usize = 8;

// --- The quartet, in mark-word bits 16..31 -----------------------------------
//
// `kind`, `element_type`, `gc_age` and `gc_flags` live in the mark word's top
// two bytes -- the object's bytes 6 and 7 -- which no lock/hash/forward state
// touches. (Until 2026-09-24 the mark word was a u64 at offset 8 and these
// were its bits 48..63, i.e. the object's bytes 14 and 15; the byte-level
// packing is unchanged, only the displacement moved.)
//
// # Bit diagram (mark bit 31 on the left)
//
//   31    28 27    24 23  22 21      18 17 16 | 15 ........... 2 | 1  0
//   +--------+--------+------+----------+-----+-----------------+------+
//   | gc_age |gc_flags| rsvd | elem_typ | knd | state payload   | state|
//   +--------+--------+------+----------+-----+-----------------+------+
//   \--------------- MARK_QUARTET_MASK -------/
//
// For a **compact instance** bits 18..23 (element type + reserved) carry the
// high part of its identity hash instead (`MARK_SHORT_HASH_HI_MASK`): a plain
// object has no element type, and its `kind` bits alone say it is one. Every
// consumer that means "is this a plain object" therefore tests the `kind`
// bits (`KIND_TAG_BYTE_MASK`), never the whole byte.
pub const MARK_QUARTET_SHIFT: u32 = 16;
/// The mark word's top 16 bits (16..31).
pub const MARK_QUARTET_MASK: u32 = 0xFFFF << MARK_QUARTET_SHIFT;

const KIND_SHIFT: u32 = MARK_QUARTET_SHIFT; // 16: byte 6 bits 0..1
const KIND_BITS: u32 = 0x3;
const ELEM_SHIFT: u32 = MARK_QUARTET_SHIFT + 2; // 18: byte 6 bits 2..5
const ELEM_BITS: u32 = 0xF;
// Bits 22..23 (byte 6, bits 6..7) are RESERVED on arrays: zero in every array
// header, which is what lets a header screen reject a candidate array whose
// word is merely a number. On a compact instance they are hash bits.
const FLAGS_SHIFT: u32 = MARK_QUARTET_SHIFT + 8; // 24: byte 7 bits 0..3
const FLAGS_BITS: u32 = 0xF;
const AGE_SHIFT: u32 = MARK_QUARTET_SHIFT + 12; // 28: byte 7 bits 4..7
const AGE_BITS: u32 = 0xF;

/// The quartet's two reserved bits (mark bits 22..23; byte 6, bits 6..7).
/// Zero on every array and legacy instance header; on a compact instance they
/// are identity-hash bits, so a screen may only test them once it knows the
/// candidate is not a compact instance.
pub const MARK_RESERVED_MASK: u32 = 0b11 << (MARK_QUARTET_SHIFT + 6);

const _: () = assert!(MARK_RESERVED_MASK & !MARK_QUARTET_MASK == 0);
const _: () = assert!(
    MARK_RESERVED_MASK
        & ((KIND_BITS << KIND_SHIFT)
            | (ELEM_BITS << ELEM_SHIFT)
            | (FLAGS_BITS << FLAGS_SHIFT)
            | (AGE_BITS << AGE_SHIFT))
        == 0
);
const _: () = assert!((KIND_BITS << KIND_SHIFT) & !MARK_QUARTET_MASK == 0);
const _: () = assert!((ELEM_BITS << ELEM_SHIFT) & !MARK_QUARTET_MASK == 0);
const _: () = assert!((FLAGS_BITS << FLAGS_SHIFT) & !MARK_QUARTET_MASK == 0);
const _: () = assert!((AGE_BITS << AGE_SHIFT) & !MARK_QUARTET_MASK == 0);
const _: () = assert!(MARK_QUARTET_MASK & (MARK_STATE_MASK | MARK_PAYLOAD_MASK) == 0);
const _: () = assert!(
    MARK_SHORT_HASH_HI_MASK == ((ELEM_BITS << ELEM_SHIFT) | MARK_RESERVED_MASK),
    "a compact instance's high hash bits are exactly element_type + reserved"
);
const _: () = assert!(MARK_SHORT_HASH_HI_MASK & (KIND_BITS << KIND_SHIFT) == 0);
const _: () = assert!(
    (MARK_PAYLOAD_MASK >> MARK_PAYLOAD_SHIFT).count_ones()
        + (MARK_SHORT_HASH_HI_MASK >> MARK_SHORT_HASH_HI_SHIFT).count_ones()
        == SHORT_HASH_BITS
);
const _: () = assert!(
    THIN_LOCK_OWNER_MASK | THIN_LOCK_RECURSION_MASK == MARK_PAYLOAD_MASK
        && THIN_LOCK_OWNER_MASK & THIN_LOCK_RECURSION_MASK == 0
);

/// Byte offset, from the OBJECT BASE, of the byte holding [`GC_FLAG_OLD_GEN`]
/// / [`GC_FLAG_MARKED`] / [`GC_FLAG_COMPACT`] / [`GC_FLAG_HEADER`] (bits 0..3)
/// and `gc_age` (bits 4..7).
pub const GC_FLAGS_BYTE_OFFSET: usize = MARK_WORD_OFFSET + 3;

/// Byte offset, from the OBJECT BASE, of the byte holding the `kind` tag
/// (bits 0..2) and, for an array, the `element_type` tag (bits 2..6).
pub const KIND_TAGS_BYTE_OFFSET: usize = MARK_WORD_OFFSET + 2;

/// Mask selecting the `kind` tag out of the [`KIND_TAGS_BYTE_OFFSET`] byte.
/// "Is a plain object" is `byte & KIND_TAG_BYTE_MASK == 0` -- never
/// `byte == 0`, because a hashed compact instance carries hash bits above it.
pub const KIND_TAG_BYTE_MASK: u8 = 0x3;

const _: () = assert!(ObjectKind::Object as u8 == 0);
const _: () = assert!(ArrayElementType::Reference as u8 == 0);

/// The highest `gc_age` the 4-bit field can hold. Ages saturate here.
pub const MAX_GC_AGE: u8 = 15;

const _: () = assert!(std::mem::offset_of!(ObjectHeader, class_id) == 0);
const _: () = assert!(std::mem::offset_of!(ObjectHeader, mark_word) == MARK_WORD_OFFSET);
const _: () = assert!(std::mem::offset_of!(ObjectHeader, shape) == ARRAY_LENGTH_OFFSET);
const _: () = assert!(std::mem::offset_of!(ObjectHeader, aux) == IDENTITY_HASH_OFFSET);
const _: () = assert!(std::mem::size_of::<ObjectHeader>() == HEADER_SIZE);
const _: () = assert!(std::mem::align_of::<ObjectHeader>() == 8);

/// Raw `kind` tag of the object at `ptr`, without ever forming an
/// `ObjectKind`. Validate with [`object_kind_from_tag`].
///
/// # Safety
/// `ptr` must point at a mapped object header word.
#[inline(always)]
pub unsafe fn kind_tag_at(ptr: *const u8) -> u8 {
    let mark = unsafe { (ptr.add(MARK_WORD_OFFSET) as *const u32).read_unaligned() };
    ObjectHeader::kind_tag(mark)
}

/// Raw `element_type` tag of the object at `ptr`, without forming an
/// `ArrayElementType`. Validate with [`array_element_type_from_tag`].
/// Meaningful only for an array: on a compact instance these bits are hash.
///
/// # Safety
/// `ptr` must point at a mapped object header word.
#[inline(always)]
pub unsafe fn element_type_tag_at(ptr: *const u8) -> u8 {
    let mark = unsafe { (ptr.add(MARK_WORD_OFFSET) as *const u32).read_unaligned() };
    ObjectHeader::element_type_tag(mark)
}

/// Whether an object's element-type tag is acceptable for its kind tag: a
/// valid `ArrayElementType` for an array, anything for a non-array (a compact
/// instance keeps identity-hash bits there, a legacy instance zero). Header
/// screens use this instead of validating the element tag unconditionally,
/// which would reject every hashed compact instance as corrupt.
#[inline]
pub fn element_tag_ok(kind_tag: u8, elem_tag: u8) -> bool {
    kind_tag != ObjectKind::Array as u8 || array_element_type_from_tag(elem_tag).is_some()
}

/// [`element_tag_ok`] for the object at `ptr`.
///
/// # Safety
/// `ptr` must point at a mapped object header word.
#[inline]
pub unsafe fn element_type_tag_ok_at(ptr: *const u8) -> bool {
    unsafe { element_tag_ok(kind_tag_at(ptr), element_type_tag_at(ptr)) }
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
/// `ptr` must be readable for `MIN_OBJECT_SIZE` bytes and 8-aligned. The
/// caller owes that; there is no way to check it from here.
#[inline]
pub unsafe fn plausible_object_header_at(ptr: *const u8) -> bool {
    // Same order and the same bounds as the collector's own screen, so the two
    // cannot drift into disagreeing about what an object is.
    let Some(kind) = object_kind_from_tag(unsafe { kind_tag_at(ptr) }) else {
        return false;
    };
    // A filler is not an object a root can name.
    if matches!(kind, ObjectKind::HumongousFiller) {
        return false;
    }
    const MAX_PLAUSIBLE_SLOTS: u32 = 1 << 24;
    let header = unsafe { &*(ptr as *const ObjectHeader) };
    let mark = header.mark();
    match kind {
        ObjectKind::Array => {
            // An array's element-type and reserved bits are real tags.
            array_element_type_from_tag(ObjectHeader::element_type_tag(mark)).is_some()
                && mark & MARK_RESERVED_MASK == 0
                && header.raw_shape() <= i32::MAX as u32
        }
        // A compact instance keeps identity-hash bits where an array keeps its
        // element type, so there is nothing more in its header word to test.
        _ if ObjectHeader::is_short_mark(mark) => true,
        // A legacy instance's element type is always zero (`ObjectHeader::new`
        // never records one for a non-array).
        _ => {
            ObjectHeader::element_type_tag(mark) == 0
                && mark & MARK_RESERVED_MASK == 0
                && header.raw_shape() <= MAX_PLAUSIBLE_SLOTS
        }
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

/// The header at the beginning of every heap-allocated object/array.
///
/// Layout (`#[repr(C, align(8))]`, 16 bytes as a Rust type):
/// - `class_id`: offset 0 -- MUST stay there as a plain `u32` (JIT contract:
///   type guards are `CMP DWORD [recv+0], imm`).
/// - `mark_word`: offset [`MARK_WORD_OFFSET`] (4) -- lock state, identity hash
///   of a compact instance, forwarding state, and the quartet.
/// - `shape`: offset [`ARRAY_LENGTH_OFFSET`] (8) -- array length / legacy
///   field count. **Long headers only.**
/// - `aux`: offset [`IDENTITY_HASH_OFFSET`] (12) -- identity hash. **Long
///   headers only.**
///
/// A compact instance's header is only the first 8 bytes: bytes 8..16 of a
/// `&ObjectHeader` that names a compact instance are its first field(s).
/// That is why `shape` and `aux` are atomics (they may alias field storage
/// that mutators write concurrently) and private: every read goes through an
/// accessor that knows which header length the object has.
///
/// `Clone`/`Copy` are not derived (atomics are `!Copy`); header copies are
/// raw byte copies of the object.
#[repr(C, align(8))]
#[derive(Debug)]
pub struct ObjectHeader {
    /// MUST stay at offset 0 as a plain `u32` -- JIT-emitted type guards are
    /// `CMP DWORD [recv+0], imm`.
    pub class_id: ClassId,
    /// Mark word: state in bits 0..1, state payload in 2..15, quartet in
    /// 16..31 (see [`MARK_QUARTET_MASK`]).
    pub mark_word: AtomicU32,
    shape: AtomicU32,
    aux: AtomicU32,
}

/// GC flag: object resides in the old generation.
pub const GC_FLAG_OLD_GEN: u8 = 0x01;

/// GC flag: object is marked as live during major GC mark phase.
pub const GC_FLAG_MARKED: u8 = 0x02;

/// GC flag: object uses the **compact** instance layout -- a header of
/// [`COMPACT_HEADER_SIZE`] bytes and naturally-sized fields at the absolute
/// displacements of its class's registered
/// [`crate::field_layout::CompactLayout`]. Set at allocation time and never
/// cleared. An object without it is either an array or a legacy instance with
/// a long header and 16-byte `Value` cells.
pub const GC_FLAG_COMPACT: u8 = 0x04;

/// GC flag: **these bytes are a published object header.** Set by every
/// allocator at allocation time, never cleared, and preserved by every
/// mark-word transition (it lives inside [`MARK_QUARTET_MASK`]).
///
/// It exists so that no header is ever all-zero: `java/lang/Object` is
/// `ClassId(0)` and `MARK_NEUTRAL`, `ObjectKind::Object` and
/// `ArrayElementType::Reference` all encode as `0`, so without it `new
/// Object()` would publish a header word byte-for-byte identical to reclaimed,
/// zeroed arena space, which a linear heap walk cannot parse.
///
/// Its ABSENCE must never be read as "not a live object" anywhere a wrong
/// answer frees memory; see `SWEEP_NO_HEADER_FLAG` in `gc/src/gen_heap.rs`.
pub const GC_FLAG_HEADER: u8 = 0x08;

/// Mixes a class id into the high 11 bits of a compact instance's identity
/// hash, so instances of different classes do not share the 20-bit space.
#[inline(always)]
const fn class_hash_bits(class_id: u32) -> u32 {
    (class_id.wrapping_mul(0x9E37_79B9) >> 21) & 0x7FF
}

impl ObjectHeader {
    /// Construct a fresh, unlocked **long** header (array or legacy
    /// instance). The mark word is `MARK_NEUTRAL` with [`GC_FLAG_HEADER`]
    /// set; mark it compact with [`Self::set_compact_shape`] or
    /// `add_gc_flags(GC_FLAG_COMPACT)`, which also clears the second word.
    ///
    /// `element_type` is recorded for arrays only; a plain object's
    /// element-type bits are zero (they are hash bits once it is compact).
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
        let elem = if kind == ObjectKind::Array {
            element_type as u32
        } else {
            0
        };
        let mark = MARK_NEUTRAL
            | ((kind as u32) & KIND_BITS) << KIND_SHIFT
            | (elem & ELEM_BITS) << ELEM_SHIFT
            | ((GC_FLAG_HEADER as u32) & FLAGS_BITS) << FLAGS_SHIFT;
        Self {
            class_id,
            mark_word: AtomicU32::new(mark),
            shape: AtomicU32::new(shape),
            aux: AtomicU32::new(0),
        }
    }

    /// An owned copy of the header at `ptr`: every field read once (the mark
    /// word atomically). For a compact instance the copied shape/aux words are
    /// field bytes and are never consulted; for a forwarded object they are
    /// the target, which the copy's accessors follow exactly as the
    /// original's would.
    ///
    /// # Safety
    /// `ptr` must point at a readable, 8-aligned object of at least
    /// [`MIN_OBJECT_SIZE`] bytes.
    #[inline]
    pub unsafe fn snapshot(ptr: *const ObjectHeader) -> ObjectHeader {
        unsafe {
            let h = &*ptr;
            ObjectHeader {
                class_id: std::ptr::addr_of!((*ptr).class_id).read(),
                mark_word: AtomicU32::new(h.mark()),
                shape: AtomicU32::new(h.raw_shape()),
                aux: AtomicU32::new(h.raw_aux()),
            }
        }
    }

    /// The header word as it is stored in memory: `class_id` in the low 32
    /// bits, the mark word in the high 32. What an allocator emitting one
    /// qword store writes.
    #[inline]
    pub fn header_word(&self) -> u64 {
        (self.class_id.as_u32() as u64) | ((self.mark() as u64) << 32)
    }

    /// Write this header into freshly allocated, zeroed memory: the header
    /// word always, the shape word only for a long header. Never touches a
    /// compact instance's fields.
    ///
    /// # Safety
    /// `ptr` must be 8-aligned and writable for the object's full size.
    #[inline]
    pub unsafe fn write_to(&self, ptr: *mut u8) {
        unsafe {
            (ptr as *mut u64).write(self.header_word());
            if !self.is_short() {
                (ptr.add(ARRAY_LENGTH_OFFSET) as *mut u32)
                    .write(self.shape.load(Ordering::Relaxed));
                (ptr.add(IDENTITY_HASH_OFFSET) as *mut u32).write(self.aux.load(Ordering::Relaxed));
            }
        }
    }

    /// One relaxed load of the mark word.
    #[inline(always)]
    pub fn mark(&self) -> u32 {
        self.mark_word.load(Ordering::Relaxed)
    }

    // --- Header length -------------------------------------------------------

    /// Whether a mark word snapshot belongs to a **compact** instance, i.e.
    /// one whose header is [`COMPACT_HEADER_SIZE`] bytes.
    #[inline(always)]
    pub fn is_short_mark(mark: u32) -> bool {
        (mark >> KIND_SHIFT) & KIND_BITS == ObjectKind::Object as u32
            && (mark >> FLAGS_SHIFT) & (GC_FLAG_COMPACT as u32) != 0
    }

    /// Whether this is a compact instance (8-byte header, no shape word).
    #[inline(always)]
    pub fn is_short(&self) -> bool {
        Self::is_short_mark(self.mark())
    }

    /// This object's header length: [`COMPACT_HEADER_SIZE`] for a compact
    /// instance, [`HEADER_SIZE`] otherwise.
    #[inline]
    pub fn header_size(&self) -> usize {
        if self.is_short() {
            COMPACT_HEADER_SIZE
        } else {
            HEADER_SIZE
        }
    }

    /// Byte offset of this object's **payload** from its base: element data
    /// for an array, the first 16-byte cell for a legacy instance, and the
    /// compact field area for a compact instance (whose fields are addressed
    /// by their layout's absolute displacements anyway).
    #[inline]
    pub fn payload_offset(&self) -> usize {
        let mark = self.mark();
        if Self::kind_of(mark) == ObjectKind::Array {
            ARRAY_DATA_OFFSET
        } else if Self::is_short_mark(mark) {
            COMPACT_HEADER_SIZE
        } else {
            HEADER_SIZE
        }
    }

    // --- Shape word (long headers) -----------------------------------------

    /// The header whose shape word describes this object: itself, or -- when
    /// it has been forwarded away and its second word holds the target -- the
    /// copy it was forwarded to (followed transitively, a bounded number of
    /// hops). A relocating collector copies before it forwards, so the copy's
    /// header is intact whenever the source reads as forwarded.
    #[inline]
    fn shape_source(&self) -> &ObjectHeader {
        let mut h = self;
        for _ in 0..8 {
            let mark = h.mark();
            if Self::mark_state(mark) != MARK_FORWARDED || mark & MARK_FWD_SELF != 0 {
                return h;
            }
            let target = h.forwarding_address();
            // Follow only a target a collector could have installed: header
            // screens run this over candidate words that merely LOOK forwarded,
            // and dereferencing their "target" would fault where the old
            // layout (shape in the header itself) read harmless garbage.
            if target.is_null()
                || std::ptr::eq(target as *const ObjectHeader, h)
                || (target as usize) & 7 != 0
                || !crate::value::is_provenance_l1_mapped(target as u64)
            {
                return h;
            }
            // SAFETY: a forwarding target is a fully copied, live object.
            h = unsafe { &*(target as *const ObjectHeader) };
        }
        h
    }

    /// The raw shape word, with no forwarding resolution and no header-length
    /// check. For diagnostics and header-copy code only.
    #[inline]
    pub fn raw_shape(&self) -> u32 {
        self.shape.load(Ordering::Relaxed)
    }

    /// The raw aux word (a long header's identity hash), unresolved.
    #[inline]
    pub fn raw_aux(&self) -> u32 {
        self.aux.load(Ordering::Relaxed)
    }

    /// The shape word of this (long-header) object, resolved through a
    /// forward, and consistent with the mark word it was read under: a
    /// parallel evacuator claims a source (mark word) BEFORE it overwrites the
    /// second word with the target, so a shape read bracketed by two equal
    /// mark-word reads is the object's own. A changed mark word means the
    /// object was forwarded in between; the next round follows the forward.
    #[inline]
    fn resolved_shape(&self) -> u32 {
        let mut h = self;
        for _ in 0..64 {
            let before = h.mark_word.load(Ordering::Acquire);
            if Self::is_forwarded_mark(before) && before & MARK_FWD_SELF == 0 {
                let next = h.shape_source();
                if std::ptr::eq(next, h) {
                    break;
                }
                h = next;
                continue;
            }
            let v = h.shape.load(Ordering::Relaxed);
            std::sync::atomic::fence(Ordering::Acquire);
            if h.mark_word.load(Ordering::Relaxed) == before {
                return v;
            }
        }
        h.shape.load(Ordering::Relaxed)
    }

    // --- Screen-only shape resolution (gen r5w6/pin10) ----------------------

    /// Forward hops [`Self::screen_shape`] follows before it refuses a chain.
    /// A collector installs one hop (two at most: a young copy promoted in the
    /// same pause); `shape_source`'s own bound is the same 8.
    pub const SCREEN_MAX_FORWARD_HOPS: usize = 8;

    /// Re-reads [`Self::screen_shape`] gives a [`MARK_FWD_BUSY`] mark before
    /// it refuses the candidate.
    ///
    /// A genuine BUSY claim lives two stores long (a parallel evacuator's
    /// claim CAS, then its target store and the mark store). No screen runs
    /// concurrently with a phase that claims: mutators are stopped for it and
    /// the workers size through their own owned-header ladder. So a candidate
    /// still BUSY after this many reads is bytes that only LOOK like a claim --
    /// in practice the high half of a heap pointer on a heap whose address has
    /// `0b1011` in bits 32..35. [`Self::forwarding_address`] waits
    /// [`FORWARDING_BUSY_WAIT_LIMIT`] rounds and `resolved_shape` asks it 64
    /// times, 262 144 spin rounds per candidate; a screen must not pay that.
    pub const SCREEN_BUSY_REREADS: u32 = 64;

    /// The shape word of a CANDIDATE header -- bytes a screen has not yet
    /// proved to be an object -- resolved the way [`Self::num_slots`] and
    /// [`Self::array_length`] resolve it, except that no forward is followed
    /// unless the caller's `inside` accepts its target, and every refusal is an
    /// answer (`None`) rather than a dereference or a spin.
    ///
    /// `None` when:
    /// - the mark reads FORWARDED (not SELF) and the target is null, this
    ///   header itself, misaligned, beyond [`crate::plausible_heap_pointer`],
    ///   or refused by `inside` -- no collector installs any of those, so the
    ///   bytes are not a forwarded object;
    /// - the chain is longer than [`Self::SCREEN_MAX_FORWARD_HOPS`];
    /// - the mark stays [`MARK_FWD_BUSY`] for [`Self::SCREEN_BUSY_REREADS`]
    ///   reads (counted in [`Self::screen_busy_declined`]).
    ///
    /// `Some(shape)` otherwise: the shape word of the header the chain ends
    /// at, read between two equal mark reads exactly as `resolved_shape`
    /// reads it. For a real object (forwarded or not) that is the value
    /// `num_slots` / `array_length` would return; for a compact instance it is
    /// a field and means nothing, but the `Some` still says its forward chain
    /// (if any) stays inside.
    ///
    /// `inside(t)` must answer `true` only for an address at which a whole
    /// [`ObjectHeader`] is readable (mapped and 8-aligned) -- the caller's
    /// heap, bounded by its commit map. A screen that uses the returned value
    /// instead of calling `num_slots` / `array_length` again also closes the
    /// window in which a concurrently rewritten candidate could read forwarded
    /// on the second read.
    ///
    /// Why this exists: the typed accessors trust
    /// [`crate::plausible_heap_pointer`] alone, which is right for a real
    /// object and a wild read for a screen. The netty `SIGSEGV
    /// addr=0x100000004` was a screen following a `Value::Object` payload
    /// word's "forward" to `0x1_0000_0000`
    /// (`docs/internal/gc/gengc-r5w5-pin9-header-screens-follow-forwards-out-of-the-heap-FIXED-20260928.md`).
    pub fn screen_shape(&self, inside: impl Fn(usize) -> bool) -> Option<u32> {
        let mut h = self;
        let mut hops = 0usize;
        let mut busy_reads = 0u32;
        let mut bracket_rounds = 0u32;
        loop {
            let before = h.mark_word.load(Ordering::Acquire);
            if Self::is_forwarded_mark(before) && before & MARK_FWD_SELF == 0 {
                if before & MARK_FWD_BUSY != 0 {
                    busy_reads += 1;
                    if busy_reads >= Self::SCREEN_BUSY_REREADS {
                        SCREEN_BUSY_DECLINED.fetch_add(1, Ordering::Relaxed);
                        return None;
                    }
                    std::hint::spin_loop();
                    continue;
                }
                if hops >= Self::SCREEN_MAX_FORWARD_HOPS {
                    return None;
                }
                // Published before the non-BUSY FORWARDED mark this `Acquire`
                // load saw (`set_forwarding_address` / `publish_claimed_forwarding`
                // store the target, then the mark with `Release`).
                let target = h.second_word().load(Ordering::Relaxed) as usize;
                if target == 0
                    || target == h as *const ObjectHeader as usize
                    || target & 7 != 0
                    || !crate::plausible_heap_pointer(target as u64)
                    || !inside(target)
                {
                    return None;
                }
                hops += 1;
                // SAFETY: `inside` accepted `target` as an address at which a
                // whole, 8-aligned header is readable (this function's
                // contract), and the check above proved the alignment again.
                h = unsafe { &*(target as *const ObjectHeader) };
                continue;
            }
            let v = h.shape.load(Ordering::Relaxed);
            std::sync::atomic::fence(Ordering::Acquire);
            if h.mark_word.load(Ordering::Relaxed) == before {
                return Some(v);
            }
            // The mark moved under the read (a thin lock, a hash install, or a
            // forward): re-read, bounded like `resolved_shape`.
            bracket_rounds += 1;
            if bracket_rounds >= 64 {
                return Some(h.shape.load(Ordering::Relaxed));
            }
        }
    }

    /// Candidates [`Self::screen_shape`] refused because their mark stayed
    /// [`MARK_FWD_BUSY`]. A healthy run reads 0 or close to it; a large count
    /// is the "high half of a heap pointer reads BUSY" population (ASLR nibble
    /// `0b1011`), which used to cost 262 144 spin rounds each.
    pub fn screen_busy_declined() -> u64 {
        SCREEN_BUSY_DECLINED.load(Ordering::Relaxed)
    }

    /// How often [`Self::forwarding_address`]'s BUSY wait gave up
    /// ([`FORWARDING_BUSY_WAIT_ABANDONED`], which `mod heap_types` being
    /// private leaves unreachable from other crates by name).
    pub fn forwarding_busy_wait_abandoned() -> u64 {
        FORWARDING_BUSY_WAIT_ABANDONED.load(Ordering::Relaxed)
    }

    /// An array's length; 0 for anything else.
    #[inline]
    pub fn array_length(&self) -> u32 {
        if self.kind() == ObjectKind::Array {
            self.resolved_shape()
        } else {
            0
        }
    }

    #[inline]
    pub fn set_array_length(&mut self, length: u32) {
        debug_assert_eq!(self.kind(), ObjectKind::Array);
        self.shape.store(length, Ordering::Relaxed);
    }

    /// The instance-field count: the class's registered compact field count
    /// for a compact instance (0 if its layout is not registered -- a walk
    /// that reaches one sizes it through [`crate::object_body_size`]'s
    /// implausible sentinel instead), the shape word for a legacy instance,
    /// and the length for an array.
    #[inline]
    pub fn num_slots(&self) -> u32 {
        if self.is_short() {
            crate::field_layout::compact_field_count(self.class_id.as_u32()).unwrap_or(0)
        } else {
            self.resolved_shape()
        }
    }

    /// Set a long header's shape word.
    #[inline]
    pub fn set_num_slots(&mut self, slots: u32) {
        debug_assert!(!self.is_short(), "a compact instance has no shape word");
        self.shape.store(slots, Ordering::Relaxed);
    }

    /// Mark an object as a compact instance of its class's registered layout.
    ///
    /// Clears the shape and aux words: on a compact instance those bytes are
    /// its first field(s), so an allocator that writes the whole 16-byte
    /// struct into zeroed memory must write zeroes there.
    #[inline]
    pub fn set_compact_shape(&mut self, _slots: u32, total_size: usize) {
        assert_eq!(total_size & 7, 0, "compact object must be 8-byte aligned");
        self.add_gc_flags(GC_FLAG_COMPACT);
    }

    // --- Forwarding ----------------------------------------------------------

    /// The second word as a 64-bit atomic (the forwarding target's home).
    #[inline(always)]
    fn second_word(&self) -> &AtomicU64 {
        // SAFETY: `shape` is at offset 8 of an 8-aligned 16-byte struct, and
        // `shape`/`aux` are both atomics, so the 8 bytes are one aligned,
        // shared-mutable word.
        unsafe { &*(&self.shape as *const AtomicU32 as *const AtomicU64) }
    }

    /// Returns true if this object has been forwarded by the GC (including a
    /// self-forward).
    #[inline]
    pub fn is_forwarded(&self) -> bool {
        Self::is_forwarded_mark(self.mark())
    }

    /// Whether this object is forwarded to itself.
    #[inline]
    pub fn is_self_forwarded(&self) -> bool {
        let mark = self.mark();
        Self::is_forwarded_mark(mark) && mark & MARK_FWD_SELF != 0
    }

    /// The forwarding address, or null if not forwarded.
    ///
    /// A self-forward answers this header's own address. A claim still in
    /// progress ([`MARK_FWD_BUSY`]) is waited out: its target is being written
    /// by the parallel evacuator that won the claim, two stores after the
    /// claim.
    ///
    /// The wait is BOUNDED. Header screens and stale-slot paths call this on
    /// words that only look forwarded, and an arbitrary word whose low nibble
    /// is `FORWARDED | BUSY` would otherwise hang the pause. Past
    /// [`FORWARDING_BUSY_WAIT_LIMIT`] bounded CPU-relax rounds the word is
    /// answered as not forwarded. In particular, this helper never repeatedly
    /// calls into the OS scheduler: a stalled owner must not turn every
    /// conservative header screen into a `sched_yield` livelock.
    pub fn forwarding_address(&self) -> *mut u8 {
        let mut rounds: u32 = 0;
        loop {
            let mark = self.mark_word.load(Ordering::Acquire);
            if !Self::is_forwarded_mark(mark) {
                return std::ptr::null_mut();
            }
            if mark & MARK_FWD_SELF != 0 {
                return self as *const ObjectHeader as *mut u8;
            }
            if mark & MARK_FWD_BUSY != 0 {
                rounds += 1;
                if rounds >= FORWARDING_BUSY_WAIT_LIMIT {
                    FORWARDING_BUSY_WAIT_ABANDONED.fetch_add(1, Ordering::Relaxed);
                    return std::ptr::null_mut();
                }
                std::hint::spin_loop();
                continue;
            }
            return self.second_word().load(Ordering::Relaxed) as usize as *mut u8;
        }
    }

    /// Install `target` as this object's relocation address (single-threaded
    /// installers: every STW serial path).
    ///
    /// # Ordering contract
    ///
    /// For a target other than the object itself this **overwrites the
    /// object's second word** -- its first field, or a long header's shape and
    /// identity hash -- and replaces the mark word's lock state. The caller
    /// must have copied the object FIRST; the copy then carries everything.
    /// A self-forward writes only the mark word.
    #[track_caller]
    pub fn set_forwarding_address(&self, target: *mut u8) {
        let prev = self.mark();
        if std::ptr::eq(target as *const ObjectHeader, self) {
            self.mark_word.store(
                Self::quartet_of(prev) | MARK_FORWARDED | MARK_FWD_SELF,
                Ordering::Release,
            );
            return;
        }
        Self::assert_forwarding_target(target as usize, prev);
        crate::value::record_object_ref_payload(target);
        // The target overwrites a live body word; a reader that sees it must
        // also see the mark word no longer describing that body (see
        // `resolved_shape`, which brackets its shape read with two mark reads).
        std::sync::atomic::fence(Ordering::Release);
        self.second_word()
            .store(target as usize as u64, Ordering::Relaxed);
        self.mark_word
            .store(Self::quartet_of(prev) | MARK_FORWARDED, Ordering::Release);
    }

    /// Parallel evacuation, step 1: claim this object by CASing its mark word
    /// from `observed` to `FORWARDED | BUSY`. `Err(current)` means another
    /// thread changed the word first (usually: won the claim).
    #[inline]
    pub fn try_claim_forwarding(&self, observed: u32) -> Result<(), u32> {
        self.mark_word
            .compare_exchange(
                observed,
                Self::quartet_of(observed) | MARK_FORWARDED | MARK_FWD_BUSY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
    }

    /// Parallel evacuation, step 2: publish the target of a claim this thread
    /// won with [`Self::try_claim_forwarding`]. A target equal to the object
    /// itself publishes a self-forward and leaves the second word alone.
    #[track_caller]
    pub fn publish_claimed_forwarding(&self, target: *mut u8) {
        let prev = self.mark();
        debug_assert!(
            Self::is_forwarded_mark(prev) && prev & MARK_FWD_BUSY != 0,
            "publish without a claim: mark={prev:#010x}"
        );
        if std::ptr::eq(target as *const ObjectHeader, self) {
            self.mark_word.store(
                Self::quartet_of(prev) | MARK_FORWARDED | MARK_FWD_SELF,
                Ordering::Release,
            );
            return;
        }
        Self::assert_forwarding_target(target as usize, prev);
        crate::value::record_object_ref_payload(target);
        // Orders the claim (the CAS in `try_claim_forwarding`) before the
        // target store, for readers that see the target and then re-read the
        // mark word (`resolved_shape`). A compiler fence on x86.
        std::sync::atomic::fence(Ordering::Release);
        self.second_word()
            .store(target as usize as u64, Ordering::Relaxed);
        self.mark_word
            .store(Self::quartet_of(prev) | MARK_FORWARDED, Ordering::Release);
    }

    /// Claim-and-publish a self-forward in one CAS from `observed`.
    #[inline]
    pub fn try_self_forward(&self, observed: u32) -> Result<(), u32> {
        self.mark_word
            .compare_exchange(
                observed,
                Self::quartet_of(observed) | MARK_FORWARDED | MARK_FWD_SELF,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
    }

    /// Undo a self-forward exactly: the state and payload (bits 0..15) and a
    /// compact instance's hash-high bits come back from `saved`, the mark word
    /// read before the self-forward; the current quartet keeps any flag or age
    /// change made since. A self-forward keeps only the quartet, and both a
    /// compact instance's identity hash and a thin lock live in the bits it
    /// drops, so [`Self::retire_forwarding`]'s `NEUTRAL` would change the
    /// hash of an object that never moved and release a lock a parked thread
    /// still holds. An INFLATED word is restored as is: its monitor is keyed
    /// by the address, which did not change.
    pub fn restore_self_forwarded_mark(&self, saved: u32) {
        // A saved word that is itself FORWARDED is not a pre-forward state (the
        // object was self-forwarded twice, the second save capturing the first
        // forward); writing it back would leave the object reading as
        // forwarded, with its field data taken for a target.
        if Self::is_forwarded_mark(saved) {
            return;
        }
        let hash_hi = if Self::is_short_mark(saved) {
            MARK_SHORT_HASH_HI_MASK
        } else {
            0
        };
        let _ = self
            .mark_word
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                Some(
                    (cur & MARK_QUARTET_MASK & !hash_hi)
                        | (saved & hash_hi)
                        | (saved & !MARK_QUARTET_MASK),
                )
            });
    }

    /// Return a forwarded header to `NEUTRAL`, keeping its quartet. For a
    /// forward-away, the second word (a long header's shape and hash, a
    /// compact instance's first field) is restored from the copy, so the
    /// retired source stays walkable at its real size.
    pub fn retire_forwarding(&self) {
        self.retire_forwarding_from(std::ptr::null_mut());
    }

    /// [`Self::retire_forwarding`], restoring the second word from `recorded`
    /// -- the target the collector RECORDED for this forward -- rather than
    /// from the target word in the header, when `recorded` is non-null. The
    /// header's word is a body word of the abandoned source, so a stray store
    /// into it during the pause would otherwise be followed as a pointer.
    pub fn retire_forwarding_from(&self, recorded: *mut u8) {
        let mark = self.mark();
        if !Self::is_forwarded_mark(mark) {
            return;
        }
        if mark & MARK_FWD_SELF == 0 {
            let target = if recorded.is_null() {
                self.forwarding_address()
            } else {
                recorded
            };
            if !target.is_null() {
                // SAFETY: the target is the fully copied object.
                let copy = unsafe { &*(target as *const ObjectHeader) };
                let word = copy.second_word().load(Ordering::Relaxed);
                self.second_word().store(word, Ordering::Relaxed);
            }
        }
        self.mark_word
            .store(Self::quartet_of(mark) | MARK_NEUTRAL, Ordering::Release);
    }

    #[inline(always)]
    #[track_caller]
    fn assert_forwarding_target(target: usize, prev: u32) {
        assert!(
            target & 7 == 0 && crate::plausible_heap_pointer(target as u64),
            "forwarding target must be a non-null, 8-byte aligned plausible user-space pointer: target={target:#018x} prev={prev:#010x}"
        );
    }

    // --- The quartet ------------------------------------------------------------

    /// This object's kind.
    #[inline(always)]
    pub fn kind(&self) -> ObjectKind {
        Self::kind_of(self.mark())
    }

    /// `kind` decoded from a mark-word snapshot. Total: the unmapped `0b11`
    /// decodes to `Object`; screens that must reject it use [`Self::kind_tag`].
    #[inline(always)]
    pub fn kind_of(mark: u32) -> ObjectKind {
        match (mark >> KIND_SHIFT) & KIND_BITS {
            1 => ObjectKind::Array,
            2 => ObjectKind::HumongousFiller,
            _ => ObjectKind::Object,
        }
    }

    /// The raw 2-bit kind tag from a snapshot.
    #[inline(always)]
    pub fn kind_tag(mark: u32) -> u8 {
        ((mark >> KIND_SHIFT) & KIND_BITS) as u8
    }

    /// The raw 4-bit `element_type` tag from a snapshot. Meaningful only for
    /// an array (a compact instance keeps hash bits there).
    #[inline(always)]
    pub fn element_type_tag(mark: u32) -> u8 {
        ((mark >> ELEM_SHIFT) & ELEM_BITS) as u8
    }

    /// This object's array element type; `Reference` for a non-array.
    #[inline(always)]
    pub fn element_type(&self) -> ArrayElementType {
        let mark = self.mark();
        if Self::kind_of(mark) != ObjectKind::Array {
            return ArrayElementType::Reference;
        }
        array_element_type_from_tag(Self::element_type_tag(mark))
            .unwrap_or(ArrayElementType::Reference)
    }

    /// `gc_age` decoded from a mark-word snapshot.
    #[inline(always)]
    pub fn gc_age_of(mark: u32) -> u8 {
        ((mark >> AGE_SHIFT) & AGE_BITS) as u8
    }

    /// Number of minor collections this object has survived.
    #[inline(always)]
    pub fn gc_age(&self) -> u8 {
        Self::gc_age_of(self.mark())
    }

    /// `mark` with its `gc_age` replaced by `age`, saturating at
    /// [`MAX_GC_AGE`]: the value [`Self::set_gc_age`] leaves in a word that
    /// held `mark`, computed without touching memory (gen r5w6/pin10).
    ///
    /// For an evacuator building a destination header it alone can see: one
    /// plain store of `mark_with_gc_age(snapshot, age)` replaces the snapshot
    /// store plus `set_gc_age`'s compare-exchange loop.
    #[inline(always)]
    pub const fn mark_with_gc_age(mark: u32, age: u8) -> u32 {
        let age = if age > MAX_GC_AGE { MAX_GC_AGE } else { age };
        (mark & !(AGE_BITS << AGE_SHIFT)) | ((age as u32) << AGE_SHIFT)
    }

    /// `mark` with `flags` OR-ed into its `gc_flags`: the value
    /// [`Self::add_gc_flags`] leaves in a word that held `mark`, for any flag
    /// set without [`GC_FLAG_COMPACT`] (whose first setting also clears the
    /// second word, which a pure function cannot do -- hence the assertion).
    /// gen r5w6/pin10; see [`Self::mark_with_gc_age`].
    #[inline(always)]
    pub fn mark_with_gc_flags(mark: u32, flags: u8) -> u32 {
        debug_assert!(
            flags & GC_FLAG_COMPACT == 0,
            "mark_with_gc_flags cannot set GC_FLAG_COMPACT: add_gc_flags also clears the second word"
        );
        mark | (((flags as u32) & FLAGS_BITS) << FLAGS_SHIFT)
    }

    /// Set the GC age, saturating at [`MAX_GC_AGE`].
    pub fn set_gc_age(&self, age: u8) {
        let age = (age.min(MAX_GC_AGE) as u32) << AGE_SHIFT;
        let _ = self
            .mark_word
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                Some((cur & !(AGE_BITS << AGE_SHIFT)) | age)
            });
    }

    /// `gc_flags` decoded from a mark-word snapshot.
    #[inline(always)]
    pub fn gc_flags_of(mark: u32) -> u8 {
        ((mark >> FLAGS_SHIFT) & FLAGS_BITS) as u8
    }

    /// The GC flag bits.
    #[inline(always)]
    pub fn gc_flags(&self) -> u8 {
        Self::gc_flags_of(self.mark())
    }

    /// Set flag bits (one `fetch_or`). Setting [`GC_FLAG_COMPACT`] on a plain
    /// object for the first time also clears its second word -- the bytes
    /// become field storage (see [`Self::set_compact_shape`]).
    #[inline(always)]
    pub fn add_gc_flags(&self, flags: u8) {
        let prev = self.mark_word.fetch_or(
            ((flags as u32) & FLAGS_BITS) << FLAGS_SHIFT,
            Ordering::Relaxed,
        );
        if flags & GC_FLAG_COMPACT != 0
            && Self::kind_of(prev) == ObjectKind::Object
            && Self::gc_flags_of(prev) & GC_FLAG_COMPACT == 0
        {
            self.shape.store(0, Ordering::Relaxed);
            self.aux.store(0, Ordering::Relaxed);
        }
    }

    /// Clear flag bits (one `fetch_and`). Idempotent; see the collectors'
    /// unmark passes for why no claim twin exists.
    #[inline(always)]
    pub fn clear_gc_flags(&self, flags: u8) {
        self.mark_word.fetch_and(
            !(((flags as u32) & FLAGS_BITS) << FLAGS_SHIFT),
            Ordering::Relaxed,
        );
    }

    /// Replace the whole flag field.
    pub fn set_gc_flags(&self, flags: u8) {
        let bits = ((flags as u32) & FLAGS_BITS) << FLAGS_SHIFT;
        let _ = self
            .mark_word
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                Some((cur & !(FLAGS_BITS << FLAGS_SHIFT)) | bits)
            });
    }

    /// Set flag bits, reporting whether **this** call is the one that set
    /// them: exactly-once, all-or-nothing, `AcqRel` on success and `Acquire`
    /// on failure and on the initial load (a concurrent marker's claim).
    /// Never mix a sticky flag ([`GC_FLAG_COMPACT`], [`GC_FLAG_OLD_GEN`]) into
    /// a claim set.
    #[inline]
    pub fn try_add_gc_flags(&self, flags: u8) -> bool {
        debug_assert!(
            (flags as u32) & !FLAGS_BITS == 0,
            "a claim set must lie inside the 4-bit gc_flags field"
        );
        let bits = ((flags as u32) & FLAGS_BITS) << FLAGS_SHIFT;
        if bits == 0 {
            return false;
        }
        let mut cur = self.mark_word.load(Ordering::Acquire);
        loop {
            if cur & bits != 0 {
                return false;
            }
            match self.mark_word.compare_exchange_weak(
                cur,
                cur | bits,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => cur = observed,
            }
        }
    }

    /// Set `kind` and `element_type`. Allocation-time only.
    pub fn set_shape_tags(&self, kind: ObjectKind, element_type: ArrayElementType) {
        let elem = if kind == ObjectKind::Array {
            element_type as u32
        } else {
            0
        };
        let bits = ((kind as u32) & KIND_BITS) << KIND_SHIFT | (elem & ELEM_BITS) << ELEM_SHIFT;
        let cur = self.mark();
        self.mark_word.store(
            (cur & !((KIND_BITS << KIND_SHIFT) | (ELEM_BITS << ELEM_SHIFT))) | bits,
            Ordering::Relaxed,
        );
    }

    /// The quartet bits of a snapshot, for constructing a replacement word
    /// that keeps them. A compact instance's hash bits are NOT part of it:
    /// every transition built from this (thin lock, inflation, forwarding,
    /// unlock) leaves an unhashed word, and the caller that must keep a hash
    /// (inflation) displaces it first.
    #[inline(always)]
    pub fn quartet_of(mark: u32) -> u32 {
        if Self::is_short_mark(mark) {
            mark & MARK_QUARTET_MASK & !MARK_SHORT_HASH_HI_MASK
        } else {
            mark & MARK_QUARTET_MASK
        }
    }

    /// True when this header's [`MARK_RESERVED_MASK`] bits are clear, or the
    /// object is a compact instance (whose reserved bits are hash bits). A
    /// screen may use this to REJECT a candidate, never to accept one.
    #[inline]
    pub fn reserved_mark_bits_clear(&self) -> bool {
        let mark = self.mark();
        Self::is_short_mark(mark) || mark & MARK_RESERVED_MASK == 0
    }

    /// Returns true if this object is in the old generation.
    pub fn is_old_gen(&self) -> bool {
        self.gc_flags() & GC_FLAG_OLD_GEN != 0
    }

    // --- Mark-word helpers on snapshots ---------------------------------------

    /// Extract the 2-bit state field from a mark word snapshot.
    #[inline(always)]
    pub fn mark_state(mark: u32) -> u32 {
        mark & MARK_STATE_MASK
    }

    /// Whether a snapshot is an unlocked, unhashed word a thin lock may CAS
    /// from: `NEUTRAL`, empty payload, and (compact instance) no hash bits.
    #[inline(always)]
    pub fn thin_lockable(mark: u32) -> bool {
        mark & (MARK_STATE_MASK | MARK_PAYLOAD_MASK) == MARK_NEUTRAL
            && !(Self::is_short_mark(mark) && mark & MARK_SHORT_HASH_HI_MASK != 0)
    }

    /// Construct a `MARK_THIN_LOCKED` word from an owner lock slot and a
    /// recursion count (0 = held once). Both must be in range
    /// ([`MAX_THIN_LOCK_SLOT`], [`MAX_THIN_LOCK_RECURSION`]).
    #[inline(always)]
    pub fn make_thin_locked(prev: u32, slot: u32, recursion: u32) -> u32 {
        debug_assert!(slot <= MAX_THIN_LOCK_SLOT);
        debug_assert!(recursion <= MAX_THIN_LOCK_RECURSION);
        Self::quartet_of(prev)
            | MARK_THIN_LOCKED
            | ((slot << THIN_LOCK_OWNER_SHIFT) & THIN_LOCK_OWNER_MASK)
            | ((recursion << THIN_LOCK_RECURSION_SHIFT) & THIN_LOCK_RECURSION_MASK)
    }

    /// The owner lock slot of a `MARK_THIN_LOCKED` word.
    #[inline(always)]
    pub fn thin_lock_owner(mark: u32) -> u32 {
        (mark & THIN_LOCK_OWNER_MASK) >> THIN_LOCK_OWNER_SHIFT
    }

    /// The recursion count of a `MARK_THIN_LOCKED` word.
    #[inline(always)]
    pub fn thin_lock_recursion(mark: u32) -> u32 {
        (mark & THIN_LOCK_RECURSION_MASK) >> THIN_LOCK_RECURSION_SHIFT
    }

    /// Construct a `MARK_INFLATED` word. The monitor itself is found by the
    /// object's address; a compact instance's hash must have been displaced
    /// into it first (this word drops it).
    #[inline(always)]
    pub fn make_inflated(prev: u32) -> u32 {
        Self::quartet_of(prev) | MARK_INFLATED
    }

    /// Construct an unlocked, unhashed `MARK_NEUTRAL` word keeping the quartet.
    #[inline(always)]
    pub fn make_neutral(prev: u32) -> u32 {
        Self::quartet_of(prev) | MARK_NEUTRAL
    }

    /// Whether a mark word snapshot encodes a GC forwarding state.
    #[inline(always)]
    pub fn is_forwarded_mark(mark: u32) -> bool {
        Self::mark_state(mark) == MARK_FORWARDED
    }

    // --- Identity hash ----------------------------------------------------------

    /// The 20 raw hash bits a compact instance's `NEUTRAL` word carries, or 0
    /// (none installed, not neutral, or not a compact instance).
    #[inline(always)]
    pub fn short_hash_bits(mark: u32) -> u32 {
        if Self::mark_state(mark) != MARK_NEUTRAL || !Self::is_short_mark(mark) {
            return 0;
        }
        ((mark & MARK_HASH_MASK) >> MARK_HASH_SHIFT)
            | (((mark & MARK_SHORT_HASH_HI_MASK) >> MARK_SHORT_HASH_HI_SHIFT) << 14)
    }

    /// The identity hash value a compact instance of `class_id` with raw hash
    /// bits `bits` (non-zero) reports: the 20 bits plus 11 class-derived bits.
    #[inline(always)]
    pub fn short_hash_value(class_id: u32, bits: u32) -> i32 {
        ((bits & 0xF_FFFF) | (class_hash_bits(class_id) << SHORT_HASH_BITS)) as i32 & i32::MAX
    }

    /// The identity hash a compact instance's snapshot carries, as the value
    /// `System.identityHashCode` reports, or 0 for none.
    #[inline(always)]
    pub fn neutral_hash_value(class_id: u32, mark: u32) -> i32 {
        match Self::short_hash_bits(mark) {
            0 => 0,
            bits => Self::short_hash_value(class_id, bits),
        }
    }

    /// Build a compact instance's `MARK_NEUTRAL` word carrying the low 20 bits
    /// of `hash` (forced non-zero -- zero is "none").
    #[inline(always)]
    pub fn make_neutral_hashed(prev: u32, hash: i32) -> u32 {
        let bits = (hash as u32) & 0xF_FFFF;
        let bits = if bits == 0 { 1 } else { bits };
        Self::quartet_of(prev)
            | MARK_NEUTRAL
            | ((bits & 0x3FFF) << MARK_HASH_SHIFT)
            | ((bits >> 14) << MARK_SHORT_HASH_HI_SHIFT)
    }

    /// Read this object's identity hash, installing one from `mint` if it has
    /// none yet.
    ///
    /// * Long header (array, legacy instance): the aux word, installed by one
    ///   CAS from 0; always `Ok`, in any lock state.
    /// * Compact instance: the `NEUTRAL` mark word. `Err(())` means the object
    ///   is not `NEUTRAL` (thin-locked or inflated), so its hash cannot live in
    ///   the header: the caller must consult the monitor (hashing a
    ///   thin-locked compact instance inflates it) and must **not** mint.
    pub fn identity_hash(&self, mint: impl Fn() -> i32) -> Result<i32, ()> {
        let class_id = self.class_id.as_u32();
        loop {
            let mark = self.mark();
            if !Self::is_short_mark(mark) {
                let cur = self.aux.load(Ordering::Relaxed);
                if cur != 0 {
                    return Ok(cur as i32);
                }
                let h = (mint() as u32) & (i32::MAX as u32);
                let h = if h == 0 { 1 } else { h };
                return Ok(
                    match self
                        .aux
                        .compare_exchange(0, h, Ordering::Relaxed, Ordering::Relaxed)
                    {
                        Ok(_) => h as i32,
                        Err(winner) => winner as i32,
                    },
                );
            }
            if Self::mark_state(mark) != MARK_NEUTRAL {
                return Err(());
            }
            let existing = Self::short_hash_bits(mark);
            if existing != 0 {
                return Ok(Self::short_hash_value(class_id, existing));
            }
            if mark & MARK_PAYLOAD_MASK != 0 {
                return Err(());
            }
            let candidate = Self::make_neutral_hashed(mark, mint());
            if self
                .mark_word
                .compare_exchange(mark, candidate, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(Self::short_hash_value(
                    class_id,
                    Self::short_hash_bits(candidate),
                ));
            }
        }
    }

    /// The identity hash already installed in this header, without minting:
    /// the aux word of a long header, the `NEUTRAL` bits of a compact
    /// instance, 0 for none (a non-neutral compact instance's hash is in its
    /// monitor).
    #[inline]
    pub fn installed_identity_hash(&self) -> i32 {
        let mark = self.mark();
        if Self::is_short_mark(mark) {
            Self::neutral_hash_value(self.class_id.as_u32(), mark)
        } else {
            self.aux.load(Ordering::Relaxed) as i32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn header(kind: ObjectKind, elem: ArrayElementType, len: u32, slots: u32) -> ObjectHeader {
        ObjectHeader::new(ClassId::new(7), kind, elem, len, slots)
    }

    fn compact_header(class_id: u32) -> ObjectHeader {
        let mut h = ObjectHeader::new(
            ClassId::new(class_id),
            ObjectKind::Object,
            ArrayElementType::Reference,
            0,
            3,
        );
        h.set_compact_shape(3, 24);
        h
    }

    /// A 16-byte, 8-aligned scratch "object" to exercise the second word.
    #[repr(C, align(8))]
    struct Obj([u8; 32]);

    fn materialise(h: &ObjectHeader) -> Box<Obj> {
        let mut o = Box::new(Obj([0; 32]));
        unsafe { h.write_to(o.0.as_mut_ptr()) };
        o
    }

    fn view(o: &Obj) -> &ObjectHeader {
        unsafe { &*(o.0.as_ptr() as *const ObjectHeader) }
    }

    // -- Layout ----------------------------------------------------------------

    #[test]
    fn header_sizes_and_offsets() {
        assert_eq!(HEADER_SIZE, 16);
        assert_eq!(COMPACT_HEADER_SIZE, 8);
        assert_eq!(MIN_OBJECT_SIZE, 16);
        assert_eq!(MARK_WORD_OFFSET, 4);
        assert_eq!(ARRAY_LENGTH_OFFSET, 8);
        assert_eq!(NUM_SLOTS_OFFSET, 8);
        assert_eq!(IDENTITY_HASH_OFFSET, 12);
        assert_eq!(FORWARDING_TARGET_OFFSET, 8);
        assert_eq!(KIND_TAGS_BYTE_OFFSET, 6);
        assert_eq!(GC_FLAGS_BYTE_OFFSET, 7);
        assert_eq!(ARRAY_DATA_OFFSET, 16);
        assert_eq!(std::mem::size_of::<ObjectHeader>(), 16);
        assert_eq!(std::mem::offset_of!(ObjectHeader, class_id), 0);
    }

    #[test]
    fn slot_size_is_16() {
        assert_eq!(SLOT_SIZE, 16);
    }

    #[test]
    fn ref_element_size_is_8() {
        assert_eq!(REF_ELEMENT_SIZE, 8);
    }

    /// The header word is `class_id | mark << 32`, which is what the JIT's
    /// inline allocators store in one qword and what `CMP DWORD [recv+0]`
    /// type guards read the low half of.
    #[test]
    fn the_header_word_is_class_id_then_mark() {
        let h = compact_header(0x1234);
        let w = h.header_word();
        assert_eq!(w as u32, 0x1234);
        assert_eq!((w >> 32) as u32, h.mark());
        let o = materialise(&h);
        assert_eq!(u64::from_le_bytes(o.0[0..8].try_into().unwrap()), w);
        // The flags byte and kind byte are where the JIT tests them.
        assert_eq!(
            o.0[GC_FLAGS_BYTE_OFFSET] & 0xF,
            GC_FLAG_COMPACT | GC_FLAG_HEADER
        );
        assert_eq!(o.0[KIND_TAGS_BYTE_OFFSET] & KIND_TAG_BYTE_MASK, 0);
    }

    /// No header this constructor builds is all-zero: `new Object()` is
    /// `ClassId(0)` with every quartet field zero, so `GC_FLAG_HEADER` is the
    /// only thing that separates it from reclaimed arena space.
    #[test]
    fn a_published_header_word_is_never_zero() {
        let mut minimal = ObjectHeader::new(
            ClassId::new(0),
            ObjectKind::Object,
            ArrayElementType::Reference,
            0,
            0,
        );
        minimal.set_compact_shape(0, 16);
        assert_ne!(minimal.header_word(), 0);
        assert_eq!(minimal.header_word() as u32, 0);
    }

    // -- Header length -----------------------------------------------------------

    #[test]
    fn compact_instances_are_short_and_everything_else_is_long() {
        let c = compact_header(3);
        assert!(c.is_short());
        assert_eq!(c.header_size(), COMPACT_HEADER_SIZE);
        assert_eq!(c.payload_offset(), COMPACT_HEADER_SIZE);

        let legacy = header(ObjectKind::Object, ArrayElementType::Reference, 0, 4);
        assert!(!legacy.is_short());
        assert_eq!(legacy.header_size(), HEADER_SIZE);
        assert_eq!(legacy.num_slots(), 4);
        assert_eq!(legacy.payload_offset(), HEADER_SIZE);

        let arr = header(ObjectKind::Array, ArrayElementType::Int, 100, 100);
        assert!(!arr.is_short());
        assert_eq!(arr.array_length(), 100);
        assert_eq!(arr.element_type(), ArrayElementType::Int);
        assert_eq!(arr.payload_offset(), ARRAY_DATA_OFFSET);
    }

    /// Marking a header compact clears the second word, so writing the whole
    /// struct over a compact instance's fresh fields writes zeroes there.
    #[test]
    fn marking_compact_clears_the_second_word() {
        let c = compact_header(3);
        assert_eq!(c.raw_shape(), 0);
        assert_eq!(c.raw_aux(), 0);
        let o = materialise(&c);
        assert!(o.0[8..32].iter().all(|&b| b == 0));
    }

    #[test]
    fn a_long_header_writes_its_shape_word() {
        let arr = header(ObjectKind::Array, ArrayElementType::Long, 9, 9);
        let o = materialise(&arr);
        assert_eq!(u32::from_le_bytes(o.0[8..12].try_into().unwrap()), 9);
        assert_eq!(view(&o).array_length(), 9);
    }

    #[test]
    fn a_plain_object_never_records_an_element_type() {
        let h = header(ObjectKind::Object, ArrayElementType::Boolean, 0, 2);
        assert_eq!(ObjectHeader::element_type_tag(h.mark()), 0);
        assert_eq!(h.element_type(), ArrayElementType::Reference);
    }

    // -- GC flags and age ----------------------------------------------------------

    #[test]
    fn gc_flag_constants() {
        assert_eq!(GC_FLAG_OLD_GEN, 0x01);
        assert_eq!(GC_FLAG_MARKED, 0x02);
        assert_eq!(GC_FLAG_COMPACT, 0x04);
        assert_eq!(GC_FLAG_HEADER, 0x08);
    }

    #[test]
    fn flags_add_clear_and_claim() {
        let h = header(ObjectKind::Object, ArrayElementType::Reference, 0, 1);
        assert!(!h.is_old_gen());
        h.add_gc_flags(GC_FLAG_OLD_GEN);
        assert!(h.is_old_gen());
        assert!(h.try_add_gc_flags(GC_FLAG_MARKED));
        assert!(!h.try_add_gc_flags(GC_FLAG_MARKED));
        assert!(!h.try_add_gc_flags(0));
        h.clear_gc_flags(GC_FLAG_MARKED);
        assert!(h.try_add_gc_flags(GC_FLAG_MARKED));
        assert_eq!(
            h.gc_flags(),
            GC_FLAG_OLD_GEN | GC_FLAG_MARKED | GC_FLAG_HEADER
        );
    }

    #[test]
    fn every_gc_age_survives_every_transition() {
        for age in 0..=MAX_GC_AGE {
            let h = compact_header(5);
            h.set_gc_age(age);
            let m = h.mark();
            for w in [
                ObjectHeader::make_thin_locked(m, MAX_THIN_LOCK_SLOT, MAX_THIN_LOCK_RECURSION),
                ObjectHeader::make_inflated(m),
                ObjectHeader::make_neutral(m),
                ObjectHeader::make_neutral_hashed(m, -1),
            ] {
                assert_eq!(ObjectHeader::gc_age_of(w), age);
                assert_eq!(
                    ObjectHeader::gc_flags_of(w),
                    GC_FLAG_COMPACT | GC_FLAG_HEADER
                );
                assert_eq!(ObjectHeader::kind_of(w), ObjectKind::Object);
            }
        }
        let h = compact_header(5);
        h.set_gc_age(200);
        assert_eq!(h.gc_age(), MAX_GC_AGE);
    }

    // -- Thin locks -------------------------------------------------------------------

    #[test]
    fn thin_lock_words_round_trip() {
        let m = compact_header(5).mark();
        for slot in [0, 1, 0x155, MAX_THIN_LOCK_SLOT] {
            for rec in 0..=MAX_THIN_LOCK_RECURSION {
                let w = ObjectHeader::make_thin_locked(m, slot, rec);
                assert_eq!(ObjectHeader::mark_state(w), MARK_THIN_LOCKED);
                assert_eq!(ObjectHeader::thin_lock_owner(w), slot);
                assert_eq!(ObjectHeader::thin_lock_recursion(w), rec);
                assert_eq!(ObjectHeader::quartet_of(w), ObjectHeader::quartet_of(m));
                assert_eq!(ObjectHeader::short_hash_bits(w), 0);
            }
        }
    }

    #[test]
    fn a_hashed_compact_word_is_not_thin_lockable_but_a_hashed_array_is() {
        let c = compact_header(5);
        assert!(ObjectHeader::thin_lockable(c.mark()));
        c.identity_hash(|| 0x7_1234).unwrap();
        assert!(!ObjectHeader::thin_lockable(c.mark()));

        let arr = header(ObjectKind::Array, ArrayElementType::Int, 3, 3);
        let o = materialise(&arr);
        view(&o).identity_hash(|| 42).unwrap();
        assert!(ObjectHeader::thin_lockable(view(&o).mark()));
    }

    #[test]
    fn an_aged_array_is_still_thin_lockable() {
        let arr = header(ObjectKind::Array, ArrayElementType::Int, 3, 3);
        arr.set_gc_age(MAX_GC_AGE);
        arr.add_gc_flags(GC_FLAG_OLD_GEN | GC_FLAG_MARKED);
        assert!(ObjectHeader::thin_lockable(arr.mark()));
    }

    // -- Identity hash ----------------------------------------------------------------

    #[test]
    fn a_compact_hash_is_installed_once_and_then_stable() {
        let c = compact_header(11);
        let first = c.identity_hash(|| 0x0ABC_DEF1).unwrap();
        assert_ne!(first, 0);
        assert!(first >= 0);
        let again = c.identity_hash(|| 999).unwrap();
        assert_eq!(first, again);
        assert_eq!(c.installed_identity_hash(), first);
        assert_eq!(ObjectHeader::short_hash_bits(c.mark()), 0xCDEF1);
        // The quartet is untouched.
        assert!(c.is_short());
        assert_eq!(c.gc_flags(), GC_FLAG_COMPACT | GC_FLAG_HEADER);
    }

    #[test]
    fn a_minted_zero_is_never_installed() {
        let c = compact_header(11);
        let h = c.identity_hash(|| 0x10_0000).unwrap();
        assert_ne!(ObjectHeader::short_hash_bits(c.mark()), 0);
        assert_eq!(c.identity_hash(|| 5).unwrap(), h);

        let arr = header(ObjectKind::Array, ArrayElementType::Int, 1, 1);
        let o = materialise(&arr);
        assert_eq!(view(&o).identity_hash(|| 0).unwrap(), 1);
    }

    #[test]
    fn a_non_neutral_compact_instance_refuses_rather_than_minting() {
        let c = compact_header(11);
        let m = c.mark();
        c.mark_word
            .store(ObjectHeader::make_thin_locked(m, 3, 0), Ordering::Relaxed);
        assert_eq!(c.identity_hash(|| 77), Err(()));
        c.mark_word
            .store(ObjectHeader::make_inflated(m), Ordering::Relaxed);
        assert_eq!(c.identity_hash(|| 77), Err(()));
    }

    #[test]
    fn a_long_header_hash_lives_beside_the_lock_state() {
        let legacy = header(ObjectKind::Object, ArrayElementType::Reference, 0, 2);
        let o = materialise(&legacy);
        let v = view(&o);
        let h = v.identity_hash(|| 0x4455_6677).unwrap();
        assert_eq!(h, 0x4455_6677);
        let m = v.mark();
        v.mark_word
            .store(ObjectHeader::make_thin_locked(m, 9, 2), Ordering::Relaxed);
        assert_eq!(v.identity_hash(|| 1).unwrap(), h);
        assert_eq!(v.num_slots(), 2, "the hash does not disturb the shape word");
    }

    #[test]
    fn compact_hashes_of_different_classes_differ_in_their_high_bits() {
        let a = ObjectHeader::short_hash_value(1, 0x12345);
        let b = ObjectHeader::short_hash_value(2, 0x12345);
        assert_eq!(a & 0xF_FFFF, 0x12345);
        assert_eq!(b & 0xF_FFFF, 0x12345);
        assert_ne!(a, b);
        assert!(a > 0 && b > 0);
    }

    #[test]
    fn concurrent_hashers_converge_on_one_value() {
        let c = std::sync::Arc::new(compact_header(13));
        let hs: Vec<i32> = (0..8)
            .map(|i| {
                let c = c.clone();
                std::thread::spawn(move || c.identity_hash(|| 1000 + i).unwrap())
            })
            .map(|t| t.join().unwrap())
            .collect();
        assert!(hs.windows(2).all(|w| w[0] == w[1]));
    }

    #[test]
    fn inflating_or_forwarding_drops_the_compact_hash_bits() {
        let c = compact_header(11);
        c.identity_hash(|| 0xF_FFFF).unwrap();
        let m = c.mark();
        assert_eq!(
            ObjectHeader::short_hash_bits(ObjectHeader::make_inflated(m)),
            0
        );
        assert_eq!(
            ObjectHeader::make_inflated(m) & MARK_SHORT_HASH_HI_MASK,
            0,
            "an inflated compact word must not keep hash bits in byte 6"
        );
    }

    // -- Forwarding -----------------------------------------------------------------

    #[test]
    fn forwarding_writes_the_second_word_and_keeps_the_header_word() {
        let target = materialise(&header(ObjectKind::Array, ArrayElementType::Int, 33, 33));
        let src = materialise(&header(ObjectKind::Array, ArrayElementType::Int, 33, 33));
        let s = view(&src);
        let before = s.header_word();
        let t = target.0.as_ptr() as *mut u8;
        assert!(s.forwarding_address().is_null());
        s.set_forwarding_address(t);
        assert!(s.is_forwarded());
        assert!(!s.is_self_forwarded());
        assert_eq!(s.forwarding_address(), t);
        assert_eq!(s.class_id, ClassId::new(7));
        assert_eq!(s.kind(), ObjectKind::Array);
        assert_eq!(
            ObjectHeader::quartet_of(s.mark()),
            ObjectHeader::quartet_of((before >> 32) as u32)
        );
        // The shape word is gone from the source but resolves via the copy.
        assert_ne!(s.raw_shape(), 33);
        assert_eq!(s.array_length(), 33);

        s.retire_forwarding();
        assert!(!s.is_forwarded());
        assert_eq!(s.raw_shape(), 33, "retirement restores the second word");
    }

    #[test]
    fn a_self_forward_leaves_the_body_alone() {
        let mut o = materialise(&compact_header(4));
        o.0[8..16].copy_from_slice(&0xDEAD_BEEF_u64.to_le_bytes());
        let s = view(&o);
        let me = o.0.as_ptr() as *mut u8;
        s.set_forwarding_address(me);
        assert!(s.is_self_forwarded());
        assert_eq!(s.forwarding_address(), me);
        assert_eq!(
            u64::from_le_bytes(o.0[8..16].try_into().unwrap()),
            0xDEAD_BEEF
        );
        view(&o).retire_forwarding();
        assert!(!view(&o).is_forwarded());
        assert!(view(&o).is_short());
    }

    #[test]
    fn a_claim_is_won_once_and_published_after() {
        let target = materialise(&compact_header(4));
        let src = materialise(&compact_header(4));
        let s = view(&src);
        let observed = s.mark();
        assert!(s.try_claim_forwarding(observed).is_ok());
        assert!(s.try_claim_forwarding(observed).is_err());
        assert!(s.is_forwarded());
        s.publish_claimed_forwarding(target.0.as_ptr() as *mut u8);
        assert_eq!(s.forwarding_address(), target.0.as_ptr() as *mut u8);
    }

    #[test]
    fn an_unpublished_claim_is_bounded_without_scheduler_yields() {
        let src = materialise(&compact_header(4));
        let s = view(&src);
        let observed = s.mark();
        assert!(s.try_claim_forwarding(observed).is_ok());
        let before = FORWARDING_BUSY_WAIT_ABANDONED.load(Ordering::Relaxed);

        assert!(s.forwarding_address().is_null());
        assert_eq!(
            FORWARDING_BUSY_WAIT_ABANDONED.load(Ordering::Relaxed),
            before + 1,
            "a stuck claim must abandon after the bounded probe"
        );
    }

    #[test]
    fn only_the_forwarded_tag_reads_as_forwarded() {
        let m = compact_header(4).mark();
        assert!(!ObjectHeader::is_forwarded_mark(m));
        assert!(!ObjectHeader::is_forwarded_mark(
            ObjectHeader::make_thin_locked(m, 3, 3)
        ));
        assert!(!ObjectHeader::is_forwarded_mark(
            ObjectHeader::make_inflated(m)
        ));
        assert!(ObjectHeader::is_forwarded_mark(m | MARK_FORWARDED));
    }

    #[test]
    #[should_panic(expected = "forwarding target")]
    fn a_misaligned_forwarding_target_is_refused() {
        let src = materialise(&compact_header(4));
        view(&src).set_forwarding_address(0x1003 as *mut u8);
    }

    #[test]
    fn shape_source_refuses_unmapped_forwarding_target_without_faulting() {
        // An object or candidate word whose mark happens to be MARK_FORWARDED and whose
        // second word holds an aligned 64-bit value in unmapped space (e.g. 0x1_0000_0000,
        // which decodes as shape=0, aux=1).
        let raw: [u64; 2] = [(MARK_FORWARDED as u64) << 32, 0x1_0000_0000];
        let h = unsafe { &*(raw.as_ptr() as *const ObjectHeader) };
        assert_eq!(h.shape_source() as *const _, h as *const _);
        assert_eq!(h.num_slots(), 0);
        assert_eq!(h.array_length(), 0);
    }

    // -- Header screen ------------------------------------------------------------

    #[test]
    fn the_object_header_screen_separates_headers_from_numbers() {
        let obj = materialise(&header(
            ObjectKind::Object,
            ArrayElementType::Reference,
            0,
            3,
        ));
        assert!(unsafe { plausible_object_header_at(obj.0.as_ptr()) });
        let arr = materialise(&header(ObjectKind::Array, ArrayElementType::Long, 16, 16));
        assert!(unsafe { plausible_object_header_at(arr.0.as_ptr()) });
        let c = materialise(&compact_header(9));
        view(&c).identity_hash(|| 0xF_FFFF).unwrap();
        assert!(
            unsafe { plausible_object_header_at(c.0.as_ptr()) },
            "a hashed compact instance's hash bits are not a corrupt element type"
        );
        let filler = materialise(&header(
            ObjectKind::HumongousFiller,
            ArrayElementType::Reference,
            0,
            0,
        ));
        assert!(!unsafe { plausible_object_header_at(filler.0.as_ptr()) });

        let mut rejected = 0usize;
        const N: usize = 256;
        for i in 0..N {
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
        assert!(rejected * 2 >= N, "rejected only {rejected} of {N}");
    }

    #[test]
    fn primitive_array_kind_bytes_have_clear_reserved_bits() {
        for d in ["[Z", "[C", "[F", "[D", "[B", "[S", "[I", "[J"] {
            let tag = primitive_array_kind_tags_byte(d).unwrap();
            let arr = header(
                ObjectKind::Array,
                array_element_type_from_tag(tag >> 2).unwrap(),
                1,
                1,
            );
            let o = materialise(&arr);
            assert_eq!(o.0[KIND_TAGS_BYTE_OFFSET], tag, "{d}");
        }
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

    // -- gen r5w6/pin10: pure mark helpers and the screen-only resolution ------

    /// The pure helpers compute exactly what the RMW setters leave in memory,
    /// for every age (including past `MAX_GC_AGE`) and each non-compact flag.
    #[test]
    fn the_pure_mark_helpers_agree_with_the_setters() {
        let bases = [
            header(ObjectKind::Object, ArrayElementType::Reference, 0, 3).mark(),
            header(ObjectKind::Array, ArrayElementType::Int, 4, 4).mark(),
            compact_header(9).mark(),
            ObjectHeader::make_thin_locked(compact_header(9).mark(), 5, 2),
        ];
        let plain = || header(ObjectKind::Object, ArrayElementType::Reference, 0, 1);
        for base in bases {
            for age in [0u8, 1, 2, 3, 14, 15, 16, 200, 255] {
                let o = materialise(&plain());
                view(&o).mark_word.store(base, Ordering::Relaxed);
                view(&o).set_gc_age(age);
                let pure = ObjectHeader::mark_with_gc_age(base, age);
                assert_eq!(pure, view(&o).mark(), "{base:#x} {age}");
            }
            let flag_sets = [
                GC_FLAG_OLD_GEN,
                GC_FLAG_MARKED,
                GC_FLAG_HEADER,
                GC_FLAG_OLD_GEN | GC_FLAG_MARKED,
            ];
            for flags in flag_sets {
                let o = materialise(&plain());
                view(&o).mark_word.store(base, Ordering::Relaxed);
                view(&o).add_gc_flags(flags);
                let pure = ObjectHeader::mark_with_gc_flags(base, flags);
                assert_eq!(pure, view(&o).mark(), "{base:#x} {flags:#x}");
            }
        }
        // The evacuator's young-copy mark: age + 1 of the snapshot's age.
        let m = ObjectHeader::mark_with_gc_age(bases[0], 2);
        let next = ObjectHeader::mark_with_gc_age(m, ObjectHeader::gc_age_of(m).saturating_add(1));
        assert_eq!(ObjectHeader::gc_age_of(next), 3);
        let age_bits: u32 = 0xF << 28;
        assert_eq!(next & !age_bits, m & !age_bits, "only the age bits change");
    }

    /// Two whole headers, 8-aligned, side by side: `[0]` the candidate, `[1]`
    /// a possible forward target.
    #[repr(C, align(8))]
    struct Pair([u64; 4]);

    fn pair_header(p: &Pair, i: usize) -> &ObjectHeader {
        // SAFETY: `i < 2`; each half is 16 aligned bytes of `p`.
        unsafe { &*((p.0.as_ptr() as *const u8).add(i * 16) as *const ObjectHeader) }
    }

    fn set_pair(p: &mut Pair, i: usize, mark: u32, second: u64) {
        p.0[2 * i] = (u64::from(mark) << 32) | 7;
        p.0[2 * i + 1] = second;
    }

    #[test]
    fn the_screen_resolves_a_real_shape_and_a_forward_it_was_allowed_to_follow() {
        let arr = header(ObjectKind::Array, ArrayElementType::Int, 5, 5).mark();
        let mut p = Pair([0; 4]);
        // Not forwarded: its own shape, `inside` never asked.
        set_pair(&mut p, 0, arr, 5);
        let own = pair_header(&p, 0).screen_shape(|_| panic!("asked"));
        assert_eq!(own, Some(5));
        assert_eq!(own, Some(pair_header(&p, 0).array_length()));
        // Forwarded to [1], which `inside` accepts: [1]'s shape, as
        // `array_length` resolves it.
        let target = pair_header(&p, 1) as *const ObjectHeader as u64;
        set_pair(&mut p, 1, arr, 9);
        set_pair(&mut p, 0, arr | MARK_FORWARDED, target);
        // `shape_source` follows only a target in a span that has recorded a
        // reference (dev `ed3a63469`), which `set_forwarding_address` does for
        // every real forward; this hand-written one records it the same way.
        crate::value::record_object_ref_payload(target as *mut u8);
        assert_eq!(pair_header(&p, 0).array_length(), 9);
        let followed = pair_header(&p, 0).screen_shape(|t| t as u64 == target);
        assert_eq!(followed, Some(9));
        // Self-forwarded: its own shape word, no hop.
        set_pair(&mut p, 0, arr | MARK_FORWARDED | MARK_FWD_SELF, 5);
        assert_eq!(pair_header(&p, 0).screen_shape(|_| false), Some(5));
    }

    #[test]
    fn the_screen_refuses_every_forward_no_collector_installs() {
        let arr = header(ObjectKind::Array, ArrayElementType::Int, 5, 5).mark();
        let mut p = Pair([0; 4]);
        let here = pair_header(&p, 0) as *const ObjectHeader as u64;
        let target = pair_header(&p, 1) as *const ObjectHeader as u64;
        set_pair(&mut p, 1, arr, 9);
        // `0x1_0000_0000` is the netty shape: aligned and plausible, refused
        // by `inside`. The rest are refused before `inside` is asked: null,
        // misaligned, beyond 2^47, the header itself.
        let bad_targets = [
            0u64,
            0x1_0000_0000,
            0x1_0000_0004,
            1u64 << 47,
            here,
            target + 4,
        ];
        for bad in bad_targets {
            set_pair(&mut p, 0, arr | MARK_FORWARDED, bad);
            let screened = pair_header(&p, 0).screen_shape(|t| t as u64 == target);
            assert_eq!(screened, None, "{bad:#x}");
        }
        // A two-header cycle is cut at the hop bound.
        set_pair(&mut p, 0, arr | MARK_FORWARDED, target);
        set_pair(&mut p, 1, arr | MARK_FORWARDED, here);
        let both = |t: usize| t as u64 == here || t as u64 == target;
        assert_eq!(pair_header(&p, 0).screen_shape(both), None);
    }

    /// The busy-spin page
    /// (`docs/internal/gc/gengc-r5w5-pin9-busy-looking-candidates-spin-a-quarter-million-rounds-FIXED-20260928.md`):
    /// a mark that reads `FORWARDED | BUSY` and never publishes is refused
    /// after `SCREEN_BUSY_REREADS` reads, and `forwarding_address`'s
    /// 4096-round wait is never entered.
    #[test]
    fn a_busy_looking_candidate_is_refused_without_the_long_wait() {
        let arr = header(ObjectKind::Array, ArrayElementType::Long, 5, 5).mark();
        let mut p = Pair([0; 4]);
        let busy = MARK_FORWARDED | MARK_FWD_BUSY;
        set_pair(&mut p, 0, arr | busy, 0x1_0000_0000);
        let declined = ObjectHeader::screen_busy_declined();
        // `inside` accepts everything: the refusal must come from the BUSY
        // bound alone, not from the target test. (64 `spin_loop` rounds,
        // against the 262 144 of `resolved_shape` x `forwarding_address`.)
        assert_eq!(pair_header(&p, 0).screen_shape(|_| true), None);
        assert!(ObjectHeader::screen_busy_declined() > declined);
        // A legacy-instance-shaped word behaves the same.
        let legacy = header(ObjectKind::Object, ArrayElementType::Reference, 0, 1).mark();
        set_pair(&mut p, 1, legacy | busy, 0x1_0000_0000);
        assert_eq!(pair_header(&p, 1).screen_shape(|_| true), None);
    }
}
