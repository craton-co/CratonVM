use super::NotEntrantCause;

/// Only a redefinition leaves old bytecode behind; each cause names
/// itself distinctly on the pass line.
#[test]
fn only_a_redefinition_retires_old_bytecode_and_each_cause_is_named() {
    assert!(NotEntrantCause::Redefinition.retires_old_bytecode());
    assert!(!NotEntrantCause::Eviction.retires_old_bytecode());
    assert_eq!(NotEntrantCause::Redefinition.label(), "redefinition");
    assert_eq!(NotEntrantCause::Eviction.label(), "eviction");
}
