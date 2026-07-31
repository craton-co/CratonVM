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
pub mod compat;
pub mod error;
pub mod field_layout;
pub mod field_watch;
pub mod flag_groups;
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
// The JDK-only policy token. `types` is the only crate that `native-api`,
// `classloading`, `vm` and `vm-cli` all already depend on, so the shared
// `CompatibilityMode` / `ExecutionPolicy` pair lives here (design contract
// `docs/feature-designs/jdk-only-mode.md` §2). Re-exported at the crate root
// as well as under `compat::` because `vm/src/config.rs` re-exports it onward
// as `pub use cratonvm_types::compat::{CompatibilityMode, ExecutionPolicy};`
// and several call sites name it as `cratonvm_types::CompatibilityMode`.
pub use compat::{CompatibilityMode, ExecutionPolicy};
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
    flags, install as install_flags, BlockedAccessMode, EnvSource, FlagSource, GcFlags, IoFlags,
    JitFlags, LoaderFlags, MapSource, OverlaySource, VmFlags,
};
pub use float_format::{java_double_to_string, java_float_to_string};
pub use handle::{HandleScope, HandleStorage, RootedHandle};
// `mod heap_types` is private, so this list is the *only* way anything outside
// this crate can name a heap constant. A `pub const` added to `heap_types.rs`
// and left off this list is not merely inconvenient — it is unreachable from
// every other crate, i.e. an accidental default-off landing. `MARK_FORWARDED`,
// `FORWARDING_PTR_MASK` and `IDENTITY_HASH_CODE_OFFSET` were in exactly that
// state until 2026-07-26; see `arch-2026-07-26/header-shrink.md` §6.2 and the
// `every_public_heap_constant_is_reachable` test below.
pub use heap_types::{
    array_data_size, array_data_size_checked, array_element_type_from_tag, element_byte_size,
    object_kind_from_tag, ArrayElementType, ObjectHeader, ObjectKind, ARRAY_ELEMENT_TYPE_OFFSET,
    ARRAY_LENGTH_OFFSET, AUTOBOX_CLASS_ID, FIELD_CELL_PAYLOAD32_OFFSET,
    FIELD_CELL_PAYLOAD64_OFFSET, FIELD_CELL_TAG_OFFSET, FORWARDING_PTR_MASK, FORWARDING_PTR_OFFSET,
    GC_AGE_OFFSET, GC_FLAGS_OFFSET, GC_FLAG_COMPACT, GC_FLAG_MARKED, GC_FLAG_OLD_GEN, HEADER_SIZE,
    IDENTITY_HASH_CODE_OFFSET, INFLATED_PTR_MASK, MARK_FORWARDED, MARK_INFLATED, MARK_NEUTRAL,
    MARK_STATE_MASK, MARK_THIN_LOCKED, MARK_WORD_OFFSET, NUM_SLOTS_OFFSET, OBJECT_KIND_OFFSET,
    REF_ELEMENT_SIZE, REF_FIELD_SIZE, SLOT_SIZE, THIN_LOCK_OWNER_MASK, THIN_LOCK_OWNER_SHIFT,
    THIN_LOCK_RECURSION_MASK, THIN_LOCK_RECURSION_SHIFT,
};
pub use intern::{intern, intern_arc, StringPool};
pub use narrow_oop::{
    narrow_oops_enabled, ref_element_size, ref_field_size, NARROW_REF_SIZE, WIDE_REF_SIZE,
};
pub use value::{
    decode_value, decode_value_checked, encode_value, is_object_tag,
    jlong_bits_as_aligned_object_ptr, plausible_heap_pointer, read_value_atomic,
    read_value_checked, read_value_checked_atomic, write_value_atomic, ObjectRef, RawSlot,
    SlotType, Value, VALUE_MAX_DISCRIMINANT, VTAG_DOUBLE, VTAG_FLOAT, VTAG_INT, VTAG_LONG,
    VTAG_NULL, VTAG_OBJECT, VTAG_RETADDR, VTAG_UNINIT,
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

    /// `compat.rs` is only reachable because of the `pub mod compat;` above.
    /// Without it the module is orphaned: `native-api`, `classloading`, `vm`
    /// and `vm-cli` all name `cratonvm_types::compat::CompatibilityMode`, and
    /// every one of them fails to compile. The root re-export is the second
    /// half — `vm/src/config.rs` re-exports it onward, so dropping it is the
    /// same class of silent breakage the heap-constant test below guards.
    #[test]
    fn reexport_compat_policy() {
        assert_eq!(CompatibilityMode::default(), CompatibilityMode::Compatible);
        assert!(CompatibilityMode::JdkOnly.is_jdk_only());
        assert_eq!(CompatibilityMode::JdkOnly.as_str(), "jdk-only");

        let policy = ExecutionPolicy::jdk_only();
        assert!(policy.is_jdk_only());
        assert!(policy.real_jdk);
        assert!(!ExecutionPolicy::default().is_jdk_only());

        // The two paths to the same type must be the same type, not a copy.
        let via_module: compat::CompatibilityMode = CompatibilityMode::JdkOnly;
        assert_eq!(via_module, CompatibilityMode::JdkOnly);
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

    /// The re-exported heap constants, checked against the properties that the
    /// rest of the workspace actually depends on.
    ///
    /// This used to open with `assert_eq!(HEADER_SIZE, 32);`. That pinned one
    /// historical *value* rather than any invariant: it records what the header
    /// happened to be, so a deliberate, fully-correct shrink trips it and a
    /// careless change that keeps the size but breaks alignment does not. It
    /// also cannot say *why* 32 mattered, which is the only thing the person
    /// reading the failure needs.
    ///
    /// Replaced — not deleted — with the three properties that are load-bearing
    /// and that a layout change can actually violate:
    ///
    /// 1. `HEADER_SIZE` stays on the 8-byte grid. Every heap walk, the inline
    ///    TLAB cursor bump and the qword body-zeroing loop step from it, and
    ///    the 8-byte `mark_word` must stay naturally aligned for the CAS on the
    ///    lock fast path.
    /// 2. `HEADER_SIZE <= 127`. The JIT emits array element addressing as
    ///    `[base + index*scale + HEADER_SIZE]` with a **signed** `disp8`. Past
    ///    127 the byte is read back as negative and the emitted load addresses
    ///    memory *before* the object — no panic, just a wrong address. Same for
    ///    `ARRAY_LENGTH_OFFSET`, which is the `disp8` of the bounds-check load.
    /// 3. The header is big enough to hold the fields whose offsets are also
    ///    exported, so no exported offset can point outside it.
    ///
    /// `heap_types.rs` pins (2) at compile time; keeping it here as well means
    /// the constraint is stated where the constant is *published*, and the
    /// failure message names the emitter that breaks.
    #[test]
    fn reexport_heap_constants() {
        assert_eq!(
            HEADER_SIZE % 8,
            0,
            "HEADER_SIZE anchors the 8-byte object grid: heap walks, the inline \
             TLAB cursor bump and the qword body-zeroing loop all step from it"
        );
        assert!(
            HEADER_SIZE >= MARK_WORD_OFFSET + 8,
            "the 8-byte mark word must fit inside the header"
        );
        assert!(
            HEADER_SIZE <= 127,
            "HEADER_SIZE is emitted as a signed disp8 in the JIT's array element \
             addressing (jit/src/x64.rs and jit/src/ir_lower.rs); above 127 the \
             displacement byte reads as negative and the load addresses memory \
             before the object"
        );
        assert!(
            ARRAY_LENGTH_OFFSET <= 127,
            "ARRAY_LENGTH_OFFSET is the signed disp8 of the JIT's array-length \
             load; same failure mode as HEADER_SIZE above"
        );
        assert!(ARRAY_LENGTH_OFFSET > 0);
        assert!(ARRAY_LENGTH_OFFSET + 4 <= HEADER_SIZE);
        assert!(IDENTITY_HASH_CODE_OFFSET + 4 <= HEADER_SIZE);
        assert_eq!(SLOT_SIZE, 16);
        assert_eq!(REF_ELEMENT_SIZE, 8);
        assert_eq!(OBJECT_KIND_OFFSET, 4);
        assert_eq!(ARRAY_ELEMENT_TYPE_OFFSET, 5);
        assert_eq!(AUTOBOX_CLASS_ID.as_u32(), u32::MAX);
    }

    /// `mod heap_types` is private. Anything it declares `pub` but that is left
    /// off the `pub use heap_types::{…}` list above is unreachable from every
    /// other crate in the workspace — the code exists and nothing can call it,
    /// which is a default-off landing arrived at by omission rather than by
    /// choice.
    ///
    /// That is not hypothetical: `MARK_FORWARDED`, `FORWARDING_PTR_MASK` and
    /// `IDENTITY_HASH_CODE_OFFSET` all landed in `heap_types.rs` on 2026-07-26
    /// and were stranded exactly this way. Naming them here means the
    /// re-export cannot be dropped again without failing to compile.
    #[test]
    fn every_public_heap_constant_is_reachable() {
        // The mark-word encoding for a relocated object. Consumers that match
        // on `ObjectHeader::mark_state(m)` need the tag constant itself, not
        // just the `is_forwarded_mark` helper.
        assert_eq!(MARK_FORWARDED & MARK_STATE_MASK, MARK_FORWARDED);
        assert_ne!(MARK_FORWARDED, MARK_NEUTRAL);
        assert_ne!(MARK_FORWARDED, MARK_THIN_LOCKED);
        assert_ne!(MARK_FORWARDED, MARK_INFLATED);

        // The forwarding pointer occupies every bit the state tag does not, so
        // an aligned address round-trips through the mark word exactly.
        assert_eq!(FORWARDING_PTR_MASK, !MARK_STATE_MASK);
        assert_eq!(FORWARDING_PTR_MASK & MARK_STATE_MASK, 0);
        let target = 0x0000_7fff_dead_b000u64;
        assert_eq!(target & MARK_STATE_MASK, 0, "test address must be aligned");
        assert_eq!((target | MARK_FORWARDED) & FORWARDING_PTR_MASK, target);

        // The last header field that had no named constant. `jit/src/x64.rs`
        // derived its own via `offset_of!` and `vm/src/jit/helpers.rs` still
        // writes a bare `raw_ptr.add(8)`; both should use this.
        assert_eq!(IDENTITY_HASH_CODE_OFFSET % 4, 0, "dword-addressable");
        assert!(IDENTITY_HASH_CODE_OFFSET < HEADER_SIZE);
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
