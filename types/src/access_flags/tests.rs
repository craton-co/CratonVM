use super::*;

#[test]
fn overlapping_flags_agree() {
    assert_eq!(ACC_SUPER, ACC_SYNCHRONIZED);
    assert_eq!(ACC_VOLATILE, ACC_BRIDGE);
    assert_eq!(ACC_TRANSIENT, ACC_VARARGS);
    assert_eq!(ACC_MODULE, ACC_MANDATED);
}

#[test]
fn i32_copies_match() {
    assert_eq!(ACC_PUBLIC_I32, ACC_PUBLIC as i32);
    assert_eq!(ACC_STATIC_I32, ACC_STATIC as i32);
    assert_eq!(ACC_FINAL_I32, ACC_FINAL as i32);
}

#[test]
fn recognized_modifiers_includes_public_static_final() {
    assert_ne!(RECOGNIZED_MODIFIERS & ACC_PUBLIC_I32, 0);
    assert_ne!(RECOGNIZED_MODIFIERS & ACC_STATIC_I32, 0);
    assert_ne!(RECOGNIZED_MODIFIERS & ACC_FINAL_I32, 0);
}
