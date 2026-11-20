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
