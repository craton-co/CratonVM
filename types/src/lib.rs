//! Shared types for the RustJVM project.
//!
//! This crate contains foundational types used by all subsystems:
//! value representation, class identifiers, heap layout constants,
//! and error types.

pub mod access_flags;
mod class_id;
pub mod compact_value;
pub mod error;
mod heap_types;
pub mod intern;
mod value;

pub use class_id::{ClassId, ClassLoaderId};
pub use intern::{intern, intern_arc, StringPool};
pub use heap_types::{
    array_data_size, array_data_size_checked, element_byte_size, ArrayElementType, ObjectHeader,
    ObjectKind, ARRAY_LENGTH_OFFSET, AUTOBOX_CLASS_ID, GC_FLAG_MARKED, GC_FLAG_OLD_GEN,
    HEADER_SIZE, REF_ELEMENT_SIZE, SLOT_SIZE,
};
pub use compact_value::{CompactTag, CompactValue};
pub use value::{
    decode_value, encode_value, is_object_tag, jlong_bits_as_aligned_object_ptr, ObjectRef, Value,
    VTAG_DOUBLE, VTAG_FLOAT, VTAG_INT, VTAG_LONG, VTAG_NULL, VTAG_OBJECT, VTAG_RETADDR,
    VTAG_UNINIT,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that all public re-exports from lib.rs are accessible and usable.
    #[test]
    fn reexport_class_id() {
        let id = ClassId::new(42);
        assert_eq!(id.as_u32(), 42);
    }

    #[test]
    fn reexport_class_loader_id() {
        let _ = ClassLoaderId::Bootstrap;
        let _ = ClassLoaderId::Extension;
        let _ = ClassLoaderId::Application;
    }

    #[test]
    fn reexport_value_types() {
        let v = Value::Int(1);
        assert_eq!(v.as_int(), Some(1));

        let (bits, tag) = encode_value(Value::Long(99));
        assert_eq!(decode_value(bits, tag).as_long(), Some(99));
    }

    #[test]
    fn reexport_value_tags() {
        assert_eq!(VTAG_INT, 0);
        assert_eq!(VTAG_LONG, 1);
        assert_eq!(VTAG_FLOAT, 2);
        assert_eq!(VTAG_DOUBLE, 3);
        assert_eq!(VTAG_OBJECT, 4);
        assert_eq!(VTAG_NULL, 5);
        assert_eq!(VTAG_UNINIT, 6);
        assert_eq!(VTAG_RETADDR, 7);
    }

    #[test]
    fn reexport_is_object_tag() {
        assert!(is_object_tag(VTAG_OBJECT));
        assert!(!is_object_tag(VTAG_NULL));
        assert!(!is_object_tag(VTAG_INT));
    }

    #[test]
    fn reexport_heap_constants() {
        assert_eq!(HEADER_SIZE, 32);
        assert_eq!(SLOT_SIZE, 16);
        assert_eq!(REF_ELEMENT_SIZE, 8);
        assert!(ARRAY_LENGTH_OFFSET > 0);
        assert_eq!(AUTOBOX_CLASS_ID.as_u32(), 0xAB00_0000);
    }

    #[test]
    fn reexport_heap_types() {
        let _ = ObjectKind::Object;
        let _ = ObjectKind::Array;
        let _ = ArrayElementType::Int;

        assert_eq!(element_byte_size(ArrayElementType::Int), 4);
        assert_eq!(array_data_size(10, ArrayElementType::Int).unwrap(), 40);
        assert_eq!(array_data_size_checked(10, ArrayElementType::Int), Some(40));
    }

    #[test]
    fn reexport_gc_flags() {
        assert_eq!(GC_FLAG_OLD_GEN, 0x01);
        assert_eq!(GC_FLAG_MARKED, 0x02);
    }

    #[test]
    fn reexport_object_header() {
        let header = ObjectHeader {
            class_id: ClassId::new(0),
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
        };
        assert!(!header.is_forwarded());
        assert!(!header.is_old_gen());
    }

    #[test]
    fn reexport_object_ref() {
        let fake_ptr = 0x1000_u64 as *mut u8;
        let r = unsafe { ObjectRef::from_raw(fake_ptr) };
        assert_eq!(r.as_ptr() as u64, 0x1000);
    }
}
