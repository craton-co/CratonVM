// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Shared types for the CratonVM project.
//!
//! This crate contains foundational types used by all subsystems:
//! value representation, class identifiers, heap layout constants,
//! and error types.

pub mod access_flags;
mod class_id;
pub mod compact_value;
pub mod error;
pub mod field_layout;
pub mod field_watch;
pub mod flags;
pub mod float_format;
pub mod handle;
mod heap_types;
pub mod intern;
pub mod jit_activation;
pub mod loader_pin;
pub mod lock_order;
pub mod metadata_pin;
pub mod mirror_pin;
pub mod narrow_oop;
pub mod reflective_probe;
mod value;

pub use class_id::{ClassId, ClassLoaderId};
pub use compact_value::{CompactTag, CompactValue, CompactValueError};
#[cfg(any(test, debug_assertions))]
pub use field_layout::clear_class_layouts;
pub use field_layout::{
    class_layout, class_layout_for_fields, compact_field_slot, compact_field_storage,
    compact_object_body_size, compact_object_field_storage, compact_ref_fields_enabled,
    is_compact_object, layout_generation, layout_replace_guard, object_body_size,
    read_compact_field, register_class_layout, set_compact_ref_fields_enabled,
    unregister_class_layout, with_class_layout, write_compact_field, CompactLayout,
    FieldStorageKind,
};
pub use flags::{
    flags, install as install_flags, BlockedAccessMode, EnvSource, FlagSource, GcFlags, JitFlags,
    MapSource, OverlaySource, VmFlags,
};
pub use float_format::{java_double_to_string, java_float_to_string};
pub use handle::{HandleScope, HandleStorage, RootedHandle};
pub use heap_types::{
    array_data_size, array_data_size_checked, array_element_type_from_tag, element_byte_size,
    object_kind_from_tag, ArrayElementType, ObjectHeader, ObjectKind, ARRAY_ELEMENT_TYPE_OFFSET,
    ARRAY_LENGTH_OFFSET, AUTOBOX_CLASS_ID, FIELD_CELL_PAYLOAD32_OFFSET,
    FIELD_CELL_PAYLOAD64_OFFSET, FIELD_CELL_TAG_OFFSET, FORWARDING_PTR_OFFSET, GC_AGE_OFFSET,
    GC_FLAGS_OFFSET, GC_FLAG_COMPACT, GC_FLAG_MARKED, GC_FLAG_OLD_GEN, HEADER_SIZE,
    INFLATED_PTR_MASK, MARK_INFLATED, MARK_NEUTRAL, MARK_STATE_MASK, MARK_THIN_LOCKED,
    MARK_WORD_OFFSET, NUM_SLOTS_OFFSET, OBJECT_KIND_OFFSET, REF_ELEMENT_SIZE, REF_FIELD_SIZE,
    SLOT_SIZE, THIN_LOCK_OWNER_MASK, THIN_LOCK_OWNER_SHIFT, THIN_LOCK_RECURSION_MASK,
    THIN_LOCK_RECURSION_SHIFT,
};
pub use intern::{intern, intern_arc, StringPool};
pub use narrow_oop::{
    narrow_oops_enabled, ref_element_size, ref_field_size, NARROW_REF_SIZE, WIDE_REF_SIZE,
};
pub use value::{
    decode_value, decode_value_checked, encode_value, is_object_tag,
    jlong_bits_as_aligned_object_ptr, plausible_heap_pointer, read_value_atomic,
    read_value_checked, read_value_checked_atomic, write_value_atomic, ObjectRef, Value,
    VALUE_MAX_DISCRIMINANT, VTAG_DOUBLE, VTAG_FLOAT, VTAG_INT, VTAG_LONG, VTAG_NULL, VTAG_OBJECT,
    VTAG_RETADDR, VTAG_UNINIT,
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
        assert_eq!(OBJECT_KIND_OFFSET, 4);
        assert_eq!(ARRAY_ELEMENT_TYPE_OFFSET, 5);
        assert_eq!(AUTOBOX_CLASS_ID.as_u32(), u32::MAX);
    }

    #[test]
    fn reexport_heap_types() {
        let _ = ObjectKind::Object;
        let _ = ObjectKind::Array;
        let _ = ArrayElementType::Int;

        assert_eq!(element_byte_size(ArrayElementType::Int), 4);
        assert_eq!(array_data_size(10, ArrayElementType::Int).unwrap(), 40);
        assert_eq!(array_data_size_checked(10, ArrayElementType::Int), Some(40));
        assert_eq!(object_kind_from_tag(1), Some(ObjectKind::Array));
        assert_eq!(object_kind_from_tag(0x7f), None);
        assert_eq!(array_element_type_from_tag(10), Some(ArrayElementType::Int));
        assert_eq!(array_element_type_from_tag(0x7f), None);
    }

    #[test]
    fn reexport_gc_flags() {
        assert_eq!(GC_FLAG_OLD_GEN, 0x01);
        assert_eq!(GC_FLAG_MARKED, 0x02);
    }

    #[test]
    fn reexport_object_header() {
        let header = ObjectHeader::new(
            ClassId::new(0),
            ObjectKind::Object,
            ArrayElementType::Boolean,
            0,
            0,
            0,
        );
        assert!(!header.is_forwarded());
        assert!(!header.is_old_gen());
    }

    #[test]
    fn reexport_object_ref() {
        let fake_ptr = 0x1000_u64 as *mut u8;
        let r = unsafe { ObjectRef::from_raw(fake_ptr) };
        assert_eq!(r.as_ptr() as u64, 0x1000);
    }

    #[test]
    fn reexport_handle_types() {
        struct S {
            slots: Vec<Option<ObjectRef>>,
        }
        impl HandleStorage for S {
            fn root(&mut self, r: ObjectRef) -> u32 {
                self.slots.push(Some(r));
                (self.slots.len() - 1) as u32
            }
            fn unroot(&mut self, slot: u32) {
                self.slots[slot as usize] = None;
            }
            fn get(&self, slot: u32) -> ObjectRef {
                self.slots[slot as usize].unwrap()
            }
        }
        let mut storage = S { slots: Vec::new() };
        let fake_ptr = 0x2000_u64 as *mut u8;
        let r = unsafe { ObjectRef::from_raw(fake_ptr) };
        let handle = RootedHandle::new(&mut storage, r);
        assert_eq!(handle.get(&storage), r);
        let mut scope = HandleScope::new(&mut storage);
        let h2 = scope.root(r);
        assert_eq!(scope.get(&h2), r);
    }
}
