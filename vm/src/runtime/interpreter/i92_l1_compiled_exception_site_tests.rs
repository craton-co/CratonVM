use super::*;

#[test]
fn a_compiled_label_splits_into_class_method_and_descriptor() {
    assert_eq!(
        split_compiled_label("p/Q$R.run:(I)V"),
        Some((("p/Q$R", "run"), "(I)V"))
    );
    assert_eq!(split_compiled_label("NoDescriptor.run"), None);
    assert_eq!(split_compiled_label(".run:()V"), None);
}

/// No compiled activation calls the helper here, so no site is kept,
/// and whatever an earlier throw left is cleared.
#[test]
fn a_throw_no_compiled_activation_names_leaves_no_site() {
    // SAFETY: never dereferenced.
    let exc = unsafe { ObjectRef::from_raw(0x5100usize as *mut u8) };
    note_compiled_throw_site(exc, 3);
    assert!(take_compiled_throw_site().is_none());
}

#[test]
fn only_an_invoke_names_a_callee_throw() {
    // invokestatic #1 at 0, athrow at 3, return at 4.
    let frame = Frame::new(
        ClassId::new(0),
        "T".to_string(),
        "m".to_string(),
        "()V".to_string(),
        None,
        vec![0xb8, 0x00, 0x01, 0xbf, 0xb1],
        vec![],
        8,
        4,
        &[],
    );
    assert!(invokes_at(&frame, 0));
    assert!(!invokes_at(&frame, 3));
    assert!(!invokes_at(&frame, 99));
}
