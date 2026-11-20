// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use std::sync::Arc;

use crate::attribute::{Attribute, LazyAttribute};
use crate::class_access_flags::ClassAccessFlags;
use crate::class_file_version::ClassFileVersion;
use crate::constant_pool::ConstantPool;
use crate::field::ClassFileField;
use crate::method::ClassFileMethod;

/// A parsed Java `.class` file (JVM spec 4.1).
///
/// `this_class`, `super_class`, and `interfaces` are stored as `Arc<str>`
/// rather than `String`. Backing storage is the pool-interned name from
/// the constant pool's Utf8 entries (see `cratonvm_types::intern_arc`), so
/// constructing a `ClassFile` no longer allocates a fresh `String` for
/// each of these names — every clone is a refcount bump on the shared
/// pool allocation. Downstream `String`-flavoured consumers can still
/// read the name as `&str` via `Arc<str>`'s deref.
#[derive(Debug)]
pub struct ClassFile {
    pub version: ClassFileVersion,
    pub constant_pool: ConstantPool,
    pub access_flags: ClassAccessFlags,
    pub this_class: Arc<str>,
    pub super_class: Option<Arc<str>>,
    pub interfaces: Vec<Arc<str>>,
    pub fields: Vec<ClassFileField>,
    pub methods: Vec<ClassFileMethod>,
    pub attributes: Vec<LazyAttribute>,
}

impl ClassFile {
    /// Returns the source file name, if the SourceFile attribute is present.
    /// Only returns a value when the attribute has already been decoded (the
    /// reader builds `LazyAttribute::Raw` by default; consumers must call
    /// `decode(&cp)` first to force decode). Returns `None` for both
    /// "attribute absent" and "attribute still raw".
    pub fn source_file(&self) -> Option<&str> {
        self.attributes.iter().find_map(|a| match a.as_decoded() {
            Some(Attribute::SourceFile(name)) => Some(&**name),
            _ => None,
        })
    }

    /// Returns true if this class file represents an interface.
    pub fn is_interface(&self) -> bool {
        self.access_flags.contains(ClassAccessFlags::INTERFACE)
    }

    /// Returns true if this class file represents an enum.
    pub fn is_enum(&self) -> bool {
        self.access_flags.contains(ClassAccessFlags::ENUM)
    }

    /// Returns true if this class file represents an annotation.
    pub fn is_annotation(&self) -> bool {
        self.access_flags.contains(ClassAccessFlags::ANNOTATION)
    }

    /// Returns true if this class file represents a module (Java 9+).
    pub fn is_module(&self) -> bool {
        self.access_flags.contains(ClassAccessFlags::MODULE)
    }

    /// Find a method by name and descriptor.
    pub fn find_method(&self, name: &str, descriptor: &str) -> Option<&ClassFileMethod> {
        self.methods
            .iter()
            .find(|m| &*m.name == name && &*m.descriptor == descriptor)
    }

    /// Find a field by name.
    pub fn find_field(&self, name: &str) -> Option<&ClassFileField> {
        self.fields.iter().find(|f| &*f.name == name)
    }
}

#[cfg(test)]
mod tests;
