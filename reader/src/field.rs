// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use std::sync::Arc;

use crate::attribute::{Attribute, LazyAttribute};
use crate::class_access_flags::FieldAccessFlags;

/// A field in a Java class file (JVM spec 4.5).
///
/// `name` and `descriptor` are stored as shared `Arc<str>` so repeated
/// references to the same identifier across different classes share a
/// single backing allocation. The reader populates them via
/// [`cratonvm_types::intern_arc`] at parse time, so constructing a
/// `ClassFileField` from the class-file hot path is a single refcount
/// bump plus a hash-table lookup.
#[derive(Debug, Clone)]
pub struct ClassFileField {
    pub access_flags: FieldAccessFlags,
    pub name: Arc<str>,
    pub descriptor: Arc<str>,
    pub attributes: Vec<LazyAttribute>,
}

impl ClassFileField {
    pub fn is_static(&self) -> bool {
        self.access_flags.contains(FieldAccessFlags::STATIC)
    }

    pub fn is_final(&self) -> bool {
        self.access_flags.contains(FieldAccessFlags::FINAL)
    }

    pub fn is_volatile(&self) -> bool {
        self.access_flags.contains(FieldAccessFlags::VOLATILE)
    }

    /// Returns the constant value index from the ConstantValue attribute, if present.
    /// A field with ConstantValue is a compile-time constant (JVM spec §4.7.2).
    pub fn constant_value_index(&self) -> Option<u16> {
        // Only returns a value when the attribute has already been decoded.
        // Lazy attributes must be force-decoded by the caller first.
        self.attributes
            .iter()
            .find_map(|attr| match attr.as_decoded() {
                Some(Attribute::ConstantValue {
                    constant_value_index,
                }) => Some(*constant_value_index),
                _ => None,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_field(flags: FieldAccessFlags) -> ClassFileField {
        ClassFileField {
            access_flags: flags,
            name: Arc::from("testField"),
            descriptor: Arc::from("I"),
            attributes: Vec::new(),
        }
    }

    #[test]
    fn field_name_and_descriptor() {
        let f = ClassFileField {
            access_flags: FieldAccessFlags::PUBLIC,
            name: Arc::from("count"),
            descriptor: Arc::from("J"),
            attributes: Vec::new(),
        };
        assert_eq!(&*f.name, "count");
        assert_eq!(&*f.descriptor, "J");
    }

    #[test]
    fn is_static_true() {
        let f = make_field(FieldAccessFlags::PUBLIC | FieldAccessFlags::STATIC);
        assert!(f.is_static());
    }

    #[test]
    fn is_static_false() {
        let f = make_field(FieldAccessFlags::PUBLIC);
        assert!(!f.is_static());
    }

    #[test]
    fn is_final_true() {
        let f = make_field(FieldAccessFlags::FINAL);
        assert!(f.is_final());
    }

    #[test]
    fn is_final_false() {
        let f = make_field(FieldAccessFlags::PUBLIC);
        assert!(!f.is_final());
    }

    #[test]
    fn is_volatile_true() {
        let f = make_field(FieldAccessFlags::VOLATILE);
        assert!(f.is_volatile());
    }

    #[test]
    fn is_volatile_false() {
        let f = make_field(FieldAccessFlags::STATIC);
        assert!(!f.is_volatile());
    }

    #[test]
    fn combined_flags() {
        let f = make_field(
            FieldAccessFlags::PUBLIC | FieldAccessFlags::STATIC | FieldAccessFlags::FINAL,
        );
        assert!(f.is_static());
        assert!(f.is_final());
        assert!(!f.is_volatile());
    }

    #[test]
    fn field_with_attributes() {
        let f = ClassFileField {
            access_flags: FieldAccessFlags::PUBLIC,
            name: Arc::from("VALUE"),
            descriptor: Arc::from("Ljava/lang/String;"),
            attributes: vec![LazyAttribute::new_decoded(Attribute::ConstantValue {
                constant_value_index: 5,
            })],
        };
        assert_eq!(f.attributes.len(), 1);
    }
}
