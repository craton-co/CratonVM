use crate::ClassId;

/// Size of `ObjectHeader` in bytes. Must be a multiple of 8 for alignment.
pub const HEADER_SIZE: usize = 32;

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
    "ObjectHeader must be exactly 32 bytes"
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
/// Layout (32 bytes total, 8-byte aligned):
/// - `class_id`: ClassId (4 bytes)
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
#[repr(C)]
#[derive(Debug, Clone, Copy)]
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
    /// Reserved padding to maintain 32-byte header size.
    pub _gc_reserved: [u8; 2],
    /// Forwarding pointer for GC. When an object is copied during collection,
    /// the old header's forwarding_ptr is set to the new location.
    /// Null means the object has not been forwarded.
    pub forwarding_ptr: *mut u8,
}

/// GC flag: object resides in the old generation.
pub const GC_FLAG_OLD_GEN: u8 = 0x01;

/// GC flag: object is marked as live during major GC mark phase.
pub const GC_FLAG_MARKED: u8 = 0x02;

impl ObjectHeader {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to create a default ObjectHeader for testing
    fn make_header() -> ObjectHeader {
        ObjectHeader {
            class_id: ClassId::new(1),
            kind: ObjectKind::Object,
            element_type: ArrayElementType::Boolean,
            _padding: [0; 2],
            identity_hash_code: 0,
            array_length: 0,
            num_slots: 0,
            gc_age: 0,
            gc_flags: 0,
            _gc_reserved: [0; 2],
            forwarding_ptr: std::ptr::null_mut(),
        }
    }

    // -- Constants --

    #[test]
    fn header_size_is_32() {
        assert_eq!(HEADER_SIZE, 32);
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
        let header = ObjectHeader {
            class_id: ClassId::new(5),
            kind: ObjectKind::Array,
            element_type: ArrayElementType::Int,
            _padding: [0; 2],
            identity_hash_code: 12345,
            array_length: 100,
            num_slots: 0,
            gc_age: 3,
            gc_flags: 0,
            _gc_reserved: [0; 2],
            forwarding_ptr: std::ptr::null_mut(),
        };
        assert_eq!(header.kind, ObjectKind::Array);
        assert_eq!(header.element_type, ArrayElementType::Int);
        assert_eq!(header.array_length, 100);
        assert_eq!(header.identity_hash_code, 12345);
        assert_eq!(header.gc_age, 3);
    }
}
