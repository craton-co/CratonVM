use super::osr_guarded_refusal_hint;
use crate::InlineRefusal;

#[test]
fn a_receiver_resolver_refusal_points_at_the_resolver_line() {
    let r = InlineRefusal::CalleeUnresolved {
        speculated_class_id: Some(419),
    };
    let hint = osr_guarded_refusal_hint(Some(&r));
    assert!(hint.contains("inline-resolve REFUSED"), "{hint}");
    assert!(hint.contains("route=receiver="), "{hint}");
    // j27 wave 4 (lane OSRSUPER): and names the opt-in that lifts it.
    assert!(hint.contains("CRATONVM_JIT_OSR_GUARDED_SUPER_BIND=1"), "{hint}");
}

#[test]
fn other_verdicts_get_no_hint() {
    // A constant-pool miss (`speculated_class_id: None`) is a real lookup
    // failure, and an admitted plan has no refusal at all.
    let cp = InlineRefusal::CalleeUnresolved {
        speculated_class_id: None,
    };
    assert_eq!(osr_guarded_refusal_hint(Some(&cp)), "");
    assert_eq!(osr_guarded_refusal_hint(Some(&InlineRefusal::ForceInterpreted)), "");
    assert_eq!(osr_guarded_refusal_hint(None), "");
}
