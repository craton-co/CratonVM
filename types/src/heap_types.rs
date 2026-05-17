use crate::ClassId;
use std::sync::atomic::AtomicU64;

/// Size of `ObjectHeader` in bytes. Must be a multiple of 8 for alignment.
///
/// NOTE: This was increased from 32 to 40 bytes when the `mark_word` field
/// was appended to support thin-lock monitors. Downstream consumers (JIT, GC)
/// that reference `HEADER_SIZE` will pick up the new size automatically; any
/// code that hardcoded `32` must be updated.
pub const HEADER_SIZE: usize = 40;

// JIT x64 emits array element offsets as signed disp8 = HEADER_SIZE as u8.
// If HEADER_SIZE exceeds 127, disp8 wraps to negative and produces wrong
// addresses. Bump to disp32 emission in jit/src/x64.rs:6690-6813 before
// allowing HEADER_SIZE to grow beyond this limit.
const _: () = assert!(
    HEADER_SIZE <= 127,
    "HEADER_SIZE must fit in signed disp8 for JIT array access"
);

// --- Mark word (thin-lock / monitor inflation) -----------------------------
//
// The mark word is a single `AtomicU64` appended to `ObjectHeader`. It encodes
// one of three states in the low 2 bits:
//
//   00 = NEUTRAL      (no lock held)
//   01 = THIN_LOCKED  (owner thread id + recursion count in upper bits)
//   10 = INFLATED     (pointer to heap-allocated Monitor in upper 62 bits)
//   11 = reserved
//
// All transitions are performed via atomic CAS on the `mark_word` field.

/// Mark word state: no lock held. Identity hash code may live in upper bits
/// (caller-managed).
pub const MARK_NEUTRAL: u64 = 0b00;
/// Mark word state: object is thin-locked by a single thread.
pub const MARK_THIN_LOCKED: u64 = 0b01;
/// Mark word state: object's monitor has been inflated to a heap `Monitor`.
pub const MARK_INFLATED: u64 = 0b10;
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

/// Byte offset of `mark_word` within `ObjectHeader`. Documented for downstream
/// agents (JIT lock fast-path) so they can emit direct atomic loads / CAS.
///
/// Derived from the `#[repr(C)]` layout: HEADER_SIZE(40) - 8 = 32. The
/// `_const_check_mark_word_offset` assertion below pins this at compile time.
pub const MARK_WORD_OFFSET: usize = 32;

/// Size of each field/array element slot in bytes.
/// Must be >= size_of::<Value>() (which is 16 bytes: 8 for the payload + 8 for the discriminant).
pub const SLOT_SIZE: usize = 16;

/// Size of each reference array element in bytes (compact: raw pointer only).
/// Object[] elements are stored as raw 8-byte pointers (0 = null) instead of
/// 16-byte Value enums. This halves memory usage for reference arrays.
pub const REF_ELEMENT_SIZE: usize = 8;

/// Special class ID for auto-boxed primitive values in compact reference arrays.
/// When a non-Object Value (Int, Long, Float, Double) is stored in a Reference
/// array via `set_array_element`, it is automatically wrapped in a 1-field object
/// with this class ID. On read via `get_array_element`, the wrapper is detected
/// and the original Value is transparently returned.
pub const AUTOBOX_CLASS_ID: ClassId = ClassId::new(0xAB00_0000);

/// Byte offset of the `array_length` field within `ObjectHeader`.
/// Derived from the `#[repr(C)]` layout: ClassId(4) + kind(1) + element_type(1) + padding(2) + identity_hash_code(4) = 12.
/// Used by the JIT compiler for inline array length reads.
pub const ARRAY_LENGTH_OFFSET: usize = 12;

// Compile-time check that the offset is correct.
const _: () = assert!(
    std::mem::offset_of!(ObjectHeader, array_length) == ARRAY_LENGTH_OFFSET,
    "ARRAY_LENGTH_OFFSET must match ObjectHeader layout"
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
/// Reference arrays use REF_ELEMENT_SIZE (8 bytes) -- compact pointer storage.
#[inline]
pub fn element_byte_size(element_type: ArrayElementType) -> usize {
    match element_type {
        ArrayElementType::Boolean | ArrayElementType::Byte => 1,
        ArrayElementType::Char | ArrayElementType::Short => 2,
        ArrayElementType::Int | ArrayElementType::Float => 4,
        ArrayElementType::Long | ArrayElementType::Double => 8,
        ArrayElementType::Reference => REF_ELEMENT_SIZE,
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
pub fn array_data_size(length: usize, element_type: ArrayElementType) -> Result<usize, &'static str> {
    array_data_size_checked(length, element_type)
        .ok_or("array data size overflow: length * element_size exceeds usize")
}

/// Whether a heap allocation is a regular object or an array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ObjectKind {
    Object = 0,
    Array = 1,
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

/// The header stored at the beginning of every heap-allocated object/array.
///
/// Layout (40 bytes total, 8-byte aligned):
/// - `class_id`: ClassId (4 bytes) -- MUST stay at offset 0 (JIT contract)
/// - `kind`: ObjectKind (1 byte)
/// - `element_type`: ArrayElementType (1 byte, only meaningful for arrays)
/// - `_padding`: 2 bytes
/// - `identity_hash_code`: i32 (4 bytes)
/// - `array_length`: u32 (4 bytes, only meaningful for arrays)
/// - `num_slots`: u32 (4 bytes, field count for objects)
/// - `gc_age`: u8 (1 byte, times survived minor GC)
/// - `gc_flags`: u8 (1 byte, bit 0 = in old gen)
/// - `_gc_reserved`: [u8; 2] (2 bytes padding)
/// - `forwarding_ptr`: *mut u8 (8 bytes, used by GC for object relocation)
/// - `mark_word`: AtomicU64 (8 bytes, thin-lock / monitor state -- offset 32)
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
    pub _padding: [u8; 2],
    pub identity_hash_code: i32,
    pub array_length: u32,
    pub num_slots: u32,
    /// GC age -- number of times this object survived a minor GC (0..15).
    pub gc_age: u8,
    /// GC flags -- bit 0: object is in old generation.
    pub gc_flags: u8,
    /// Reserved padding to maintain header alignment.
    pub _gc_reserved: [u8; 2],
    /// Forwarding pointer for GC. When an object is copied during collection,
    /// the old header's forwarding_ptr is set to the new location.
    /// Null means the object has not been forwarded.
    pub forwarding_ptr: *mut u8,
    /// Mark word -- thin-lock owner / recursion / inflated-monitor pointer.
    /// State encoded in low 2 bits; see `MARK_NEUTRAL` / `MARK_THIN_LOCKED` /
    /// `MARK_INFLATED`. Always at byte offset `MARK_WORD_OFFSET` (= 32).
    /// Initialized to `MARK_NEUTRAL` by `ObjectHeader::new`.
    pub mark_word: AtomicU64,
}

/// GC flag: object resides in the old generation.
pub const GC_FLAG_OLD_GEN: u8 = 0x01;

/// GC flag: object is marked as live during major GC mark phase.
pub const GC_FLAG_MARKED: u8 = 0x02;

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
        Self {
            class_id,
            kind,
            element_type,
            _padding: [0; 2],
            identity_hash_code,
            array_length,
            num_slots,
            gc_age: 0,
            gc_flags: 0,
            _gc_reserved: [0; 2],
            forwarding_ptr: std::ptr::null_mut(),
            mark_word: AtomicU64::new(MARK_NEUTRAL),
        }
    }

    /// Returns true if this object has been forwarded by the GC.
    pub fn is_forwarded(&self) -> bool {
        !self.forwarding_ptr.is_null()
    }

    /// Returns the forwarding address, or null if not forwarded.
    pub fn forwarding_address(&self) -> *mut u8 {
        self.forwarding_ptr
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
        debug_assert!(
            monitor_ptr & (MARK_STATE_MASK as usize) == 0,
            "Monitor pointer must have its low 2 bits clear (>= 4-byte aligned)"
        );
        (monitor_ptr as u64) | MARK_INFLATED
    }

    /// Decode the `Monitor` pointer from a `MARK_INFLATED` mark word.
    /// Result is meaningless if the mark word is not in inflated state.
    #[inline(always)]
    pub fn inflated_monitor(mark: u64) -> *mut () {
        ((mark & INFLATED_PTR_MASK) as usize) as *mut ()
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
        assert_eq!(HEADER_SIZE, 40);
        assert_eq!(std::mem::size_of::<ObjectHeader>(), HEADER_SIZE);
    }

    #[test]
    fn slot_size_is_16() {
        assert_eq!(SLOT_SIZE, 16);
    }

    #[test]
    fn ref_element_size_is_8() {
        assert_eq!(REF_ELEMENT_SIZE, 8);
    }

    #[test]
    fn array_length_offset_matches_layout() {
        assert_eq!(
            std::mem::offset_of!(ObjectHeader, array_length),
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
        assert_eq!(element_byte_size(ArrayElementType::Reference), REF_ELEMENT_SIZE);
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
        let mut header = make_header();
        let target = 0xABCD_0000_u64 as *mut u8;
        header.forwarding_ptr = target;
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
        assert_eq!(AUTOBOX_CLASS_ID.as_u32(), 0xAB00_0000);
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
        assert_eq!(header.array_length, 100);
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
        for &p in &[0x1000usize, 0xDEAD_BEE0usize, usize::MAX & !0b11] {
            let m = ObjectHeader::make_inflated(p);
            assert_eq!(ObjectHeader::mark_state(m), MARK_INFLATED);
            assert_eq!(ObjectHeader::inflated_monitor(m) as usize, p);
        }
    }

    #[test]
    fn mark_word_offset_is_stable() {
        // The JIT lock fast-path hardcodes this offset; if it ever changes
        // both the constant and every emitter must be updated together.
        assert_eq!(MARK_WORD_OFFSET, 32);
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
        // debug_assert! catches this in debug builds; the test runs under
        // `cargo test` (debug profile) so the panic is observable.
        let _ = ObjectHeader::make_inflated(0x1001);
    }
}
