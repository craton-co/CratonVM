use super::*;
use crate::test_mock::MockNativeContext;

#[test]
fn binary_names_and_indices_line_up() {
    assert_eq!(WellKnownClass::ALL.len(), WellKnownClass::COUNT);
    for (i, which) in WellKnownClass::ALL.into_iter().enumerate() {
        assert_eq!(which.index(), i, "{which:?} index");
        assert!(!which.binary_name().is_empty());
        assert!(
            !which.binary_name().contains('.'),
            "{which:?} must use the slash-separated binary name"
        );
    }
}

#[test]
fn binary_names_are_distinct() {
    let mut names: Vec<&str> = WellKnownClass::ALL
        .into_iter()
        .map(WellKnownClass::binary_name)
        .collect();
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    assert_eq!(before, names.len(), "duplicate binary_name");
}

#[test]
fn unresolved_classes_are_not_misreported() {
    // A `MockNativeContext` loads nothing, so `class_id_by_name` always
    // misses and every query must answer `None` / `false` rather than
    // matching some other slot.
    let ctx = MockNativeContext::default();
    for which in WellKnownClass::ALL {
        assert_eq!(ctx.well_known_class_id(which), None, "{which:?}");
        assert!(!ctx.class_is(ClassId::new(0), which), "{which:?}");
    }
    assert_eq!(ctx.well_known_class(ClassId::new(0)), None);
    // The `u32::MAX` guard: never confuse a real id with the sentinel.
    assert_eq!(ctx.well_known_class(ClassId::new(u32::MAX)), None);
}
