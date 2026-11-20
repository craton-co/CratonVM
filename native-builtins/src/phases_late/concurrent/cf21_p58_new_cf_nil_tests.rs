use super::*;

#[test]
fn cf21_a_completed_null_future_owes_nil_only_over_the_real_class() {
    let null = Value::Object(None);
    // Real class (not the fabricated stub): completed with null owes NIL.
    assert!(p58_cf_owes_nil(&null, true, false));
    // Pending with a null result is the pending encoding, nothing owed.
    assert!(!p58_cf_owes_nil(&null, false, false));
    // The fabricated stub carries a `done` flag of its own.
    assert!(!p58_cf_owes_nil(&null, true, true));
    // A non-null value is stored as is.
    assert!(!p58_cf_owes_nil(&Value::Int(3), true, false));
}
