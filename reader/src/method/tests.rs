use super::*;

fn make_method(flags: MethodAccessFlags) -> ClassFileMethod {
    ClassFileMethod {
        access_flags: flags,
        name: Arc::from("testMethod"),
        descriptor: Arc::from("()V"),
        attributes: Vec::new(),
    }
}

#[test]
fn method_name_and_descriptor() {
    let m = ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC,
        name: Arc::from("main"),
        descriptor: Arc::from("([Ljava/lang/String;)V"),
        attributes: Vec::new(),
    };
    assert_eq!(&*m.name, "main");
    assert_eq!(&*m.descriptor, "([Ljava/lang/String;)V");
}

#[test]
fn is_static_true() {
    let m = make_method(MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC);
    assert!(m.is_static());
}

#[test]
fn is_static_false() {
    let m = make_method(MethodAccessFlags::PUBLIC);
    assert!(!m.is_static());
}

#[test]
fn is_native_true() {
    let m = make_method(MethodAccessFlags::NATIVE);
    assert!(m.is_native());
}

#[test]
fn is_native_false() {
    let m = make_method(MethodAccessFlags::PUBLIC);
    assert!(!m.is_native());
}

#[test]
fn is_abstract_true() {
    let m = make_method(MethodAccessFlags::ABSTRACT);
    assert!(m.is_abstract());
}

#[test]
fn is_abstract_false() {
    let m = make_method(MethodAccessFlags::PUBLIC);
    assert!(!m.is_abstract());
}

#[test]
fn is_synchronized_true() {
    let m = make_method(MethodAccessFlags::SYNCHRONIZED);
    assert!(m.is_synchronized());
}

#[test]
fn is_synchronized_false() {
    let m = make_method(MethodAccessFlags::PUBLIC);
    assert!(!m.is_synchronized());
}

#[test]
fn combined_flags() {
    let m = make_method(
        MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC | MethodAccessFlags::SYNCHRONIZED,
    );
    assert!(m.is_static());
    assert!(m.is_synchronized());
    assert!(!m.is_native());
    assert!(!m.is_abstract());
}

#[test]
fn code_returns_none_when_absent() {
    let m = make_method(MethodAccessFlags::PUBLIC);
    assert!(m.code().is_none());
}

#[test]
fn code_returns_some_when_present() {
    let code_attr = crate::attribute::CodeAttribute {
        max_stack: 2,
        max_locals: 1,
        code: crate::byte_view::ByteView::from_vec(vec![0xb1]), // return
        exception_table: Vec::new(),
        attributes: Vec::new(),
    };
    let m = ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC,
        name: Arc::from("foo"),
        descriptor: Arc::from("()V"),
        attributes: vec![LazyAttribute::new_decoded(Attribute::Code(Box::new(
            code_attr,
        )))],
    };
    let code = m.code().expect("should have Code attribute");
    assert_eq!(code.max_stack, 2);
    assert_eq!(code.max_locals, 1);
    assert_eq!(code.code, vec![0xb1]);
}
