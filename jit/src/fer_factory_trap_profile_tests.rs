use super::*;

fn profile_with(pc: usize, rows: &[(u32, u32)]) -> profile::MethodProfile {
    let mut prof = profile::MethodProfile::default();
    let mut counts = profile::ReceiverCounts::default();
    for &(class_id, n) in rows {
        counts.insert(class_id, n);
    }
    prof.receivers.insert(pc, counts);
    prof
}

#[test]
fn the_factory_class_alone_or_no_profile_admits_the_trap() {
    assert!(ir_factory_trap_profile_admits(None, 5, 63));
    assert!(ir_factory_trap_profile_admits(
        Some(&profile_with(9, &[(7, 100)])),
        5,
        63
    ));
    // A few samples are enough when they name only this class.
    assert!(ir_factory_trap_profile_admits(
        Some(&profile_with(5, &[(63, 3)])),
        5,
        63
    ));
    assert!(!ir_factory_trap_profile_admits(
        Some(&profile_with(5, &[(64, 3)])),
        5,
        63
    ));
    assert!(!ir_factory_trap_profile_admits(
        Some(&profile_with(5, &[(63, 1000), (64, 1)])),
        5,
        63
    ));
    assert!(!ir_factory_trap_profile_admits(None, 5, 0));
}
