use super::*;

/// The reader is ON, and a class the
/// class manager does not know has no init claimant, whoever asks.
#[test]
fn indy21_own_clinit_fast_is_default_on_and_an_unknown_class_has_no_claimant() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    assert!(getstatic_own_clinit_fast_enabled(&shared));
    assert!(!class_initializing_on(&shared, ClassId::new(0x7_2103), 1));
    assert!(!class_initializing_on(&shared, ClassId::new(0x7_2103), 0));
}
