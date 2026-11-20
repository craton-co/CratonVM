use super::*;

#[test]
fn hierarchy_fingerprint_is_zero_before_any_redefinition() {
    let cm = ClassManager::new(&[], &[], &[]);
    assert_eq!(hierarchy_fingerprint_in(&cm, ClassId::new(1)), 0);
}
