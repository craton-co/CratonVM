// SPDX-License-Identifier: Apache-2.0
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
mod tests {
    use super::*;
    use crate::attribute::Attribute;
    use crate::class_access_flags::{FieldAccessFlags, MethodAccessFlags};
    use crate::constant_pool::ConstantPoolEntry;

    fn make_class_file(flags: ClassAccessFlags) -> ClassFile {
        ClassFile {
            version: ClassFileVersion::JAVA_8,
            constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
            access_flags: flags,
            this_class: Arc::from("com/example/Test"),
            super_class: Some(Arc::from("java/lang/Object")),
            interfaces: Vec::new(),
            fields: Vec::new(),
            methods: Vec::new(),
            attributes: Vec::new(),
        }
    }

    #[test]
    fn is_interface() {
        let cf = make_class_file(ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT);
        assert!(cf.is_interface());
        assert!(!cf.is_enum());
    }

    #[test]
    fn is_enum() {
        let cf = make_class_file(ClassAccessFlags::ENUM | ClassAccessFlags::SUPER);
        assert!(cf.is_enum());
        assert!(!cf.is_interface());
    }

    #[test]
    fn is_annotation() {
        let cf = make_class_file(ClassAccessFlags::ANNOTATION | ClassAccessFlags::INTERFACE);
        assert!(cf.is_annotation());
    }

    #[test]
    fn is_module() {
        let cf = make_class_file(ClassAccessFlags::MODULE);
        assert!(cf.is_module());
    }

    #[test]
    fn not_interface_for_plain_class() {
        let cf = make_class_file(ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER);
        assert!(!cf.is_interface());
        assert!(!cf.is_enum());
        assert!(!cf.is_annotation());
        assert!(!cf.is_module());
    }

    #[test]
    fn source_file_present() {
        let mut cf = make_class_file(ClassAccessFlags::PUBLIC);
        cf.attributes
            .push(LazyAttribute::new_decoded(Attribute::SourceFile(
                Arc::from("Test.java"),
            )));
        assert_eq!(cf.source_file(), Some("Test.java"));
    }

    #[test]
    fn source_file_absent() {
        let cf = make_class_file(ClassAccessFlags::PUBLIC);
        assert_eq!(cf.source_file(), None);
    }

    #[test]
    fn find_method_present() {
        use std::sync::Arc;
        let mut cf = make_class_file(ClassAccessFlags::PUBLIC);
        cf.methods.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: Arc::from("main"),
            descriptor: Arc::from("([Ljava/lang/String;)V"),
            attributes: Vec::new(),
        });
        cf.methods.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: Arc::from("toString"),
            descriptor: Arc::from("()Ljava/lang/String;"),
            attributes: Vec::new(),
        });
        let m = cf
            .find_method("main", "([Ljava/lang/String;)V")
            .expect("should find main");
        assert_eq!(&*m.name, "main");
    }

    #[test]
    fn find_method_absent() {
        let cf = make_class_file(ClassAccessFlags::PUBLIC);
        assert!(cf.find_method("nonexistent", "()V").is_none());
    }

    #[test]
    fn find_method_wrong_descriptor() {
        use std::sync::Arc;
        let mut cf = make_class_file(ClassAccessFlags::PUBLIC);
        cf.methods.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC,
            name: Arc::from("foo"),
            descriptor: Arc::from("()V"),
            attributes: Vec::new(),
        });
        assert!(cf.find_method("foo", "(I)V").is_none());
    }

    #[test]
    fn find_field_present() {
        use std::sync::Arc;
        let mut cf = make_class_file(ClassAccessFlags::PUBLIC);
        cf.fields.push(ClassFileField {
            access_flags: FieldAccessFlags::PRIVATE,
            name: Arc::from("count"),
            descriptor: Arc::from("I"),
            attributes: Vec::new(),
        });
        let f = cf.find_field("count").expect("should find field");
        assert_eq!(&*f.descriptor, "I");
    }

    #[test]
    fn find_field_absent() {
        let cf = make_class_file(ClassAccessFlags::PUBLIC);
        assert!(cf.find_field("missing").is_none());
    }

    #[test]
    fn super_class_none() {
        let mut cf = make_class_file(ClassAccessFlags::PUBLIC);
        cf.super_class = None;
        assert!(cf.super_class.is_none());
    }

    #[test]
    fn this_class_name() {
        let cf = make_class_file(ClassAccessFlags::PUBLIC);
        assert_eq!(&*cf.this_class, "com/example/Test");
    }
}
