use super::class_redefined_after;
use crate::classloading::ClassId;

/// `(class, the count its last redefinition completed at)`.
fn history(class_id: ClassId) -> Option<u64> {
    match class_id.as_u32() {
        0x2561 => Some(7),
        0x2562 => Some(3),
        _ => None,
    }
}

#[test]
fn a_frame_older_than_a_copied_class_s_last_redefinition_is_refused() {
    let callee = ClassId::new(0x2561);
    // The host run: the frame began (stamp 2) before the callee's
    // retransform completed (7); a body that spliced it is refused.
    assert!(class_redefined_after(2, [callee], history));
    // A frame pushed after it may enter.
    assert!(!class_redefined_after(7, [callee], history));
    assert!(!class_redefined_after(9, [callee], history));
}

#[test]
fn classes_never_redefined_or_redefined_before_the_frame_do_not_refuse() {
    let older = ClassId::new(0x2562);
    let never = ClassId::new(0x2563);
    assert!(!class_redefined_after(5, [older, never], history));
    assert!(!class_redefined_after(0, [never], history));
    assert!(!class_redefined_after(
        0,
        std::iter::empty::<ClassId>(),
        history
    ));
    // Any one of them is enough.
    assert!(class_redefined_after(
        5,
        [older, never, ClassId::new(0x2561)],
        history
    ));
}
