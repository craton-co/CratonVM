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
