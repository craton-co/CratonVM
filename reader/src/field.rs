// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
mod tests;
