use super::compat2_mode_gate;

#[test]
fn jdk_only_always_applies_the_fix_and_never_reads_the_switch() {
    assert!(compat2_mode_gate(true, || panic!(
        "switch read under --real-jdk"
    )));
}

#[test]
fn compatible_applies_the_fix_while_the_switch_is_on() {
    // Before round 12 wave 8 both gates answered `is_jdk_only()` alone,
    // i.e. `false` here.
    assert!(compat2_mode_gate(false, || true));
}

#[test]
fn compatible_with_the_switch_off_keeps_the_old_answer() {
    assert!(!compat2_mode_gate(false, || false));
}
