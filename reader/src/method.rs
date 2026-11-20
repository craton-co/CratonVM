// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use std::sync::Arc;

use crate::attribute::{Attribute, LazyAttribute};
use crate::class_access_flags::MethodAccessFlags;

/// A method in a Java class file (JVM spec 4.6).
///
/// `name` and `descriptor` are stored as shared `Arc<str>` so repeated
/// references to the same identifier across different classes share a
/// single backing allocation. The reader populates them via
/// [`cratonvm_types::intern_arc`] at parse time, so constructing a
/// `ClassFileMethod` from the class-file hot path is a single refcount
/// bump plus a hash-table lookup.
#[derive(Debug, Clone)]
pub struct ClassFileMethod {
    pub access_flags: MethodAccessFlags,
    pub name: Arc<str>,
    pub descriptor: Arc<str>,
    pub attributes: Vec<LazyAttribute>,
}

impl ClassFileMethod {
    pub fn is_static(&self) -> bool {
        self.access_flags.contains(MethodAccessFlags::STATIC)
    }

    pub fn is_native(&self) -> bool {
        self.access_flags.contains(MethodAccessFlags::NATIVE)
    }

    pub fn is_abstract(&self) -> bool {
        self.access_flags.contains(MethodAccessFlags::ABSTRACT)
    }

    pub fn is_synchronized(&self) -> bool {
        self.access_flags.contains(MethodAccessFlags::SYNCHRONIZED)
    }

    pub fn is_bridge(&self) -> bool {
        self.access_flags.contains(MethodAccessFlags::BRIDGE)
    }

    /// Returns the Code attribute, if present and already decoded.
    /// Lazy attributes must be force-decoded by the caller first.
    pub fn code(&self) -> Option<&crate::attribute::CodeAttribute> {
        self.attributes.iter().find_map(|a| match a.as_decoded() {
            Some(Attribute::Code(code)) => Some(&**code),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests;
