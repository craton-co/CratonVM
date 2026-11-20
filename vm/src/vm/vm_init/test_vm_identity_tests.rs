use super::*;

/// gcg g2/k (`common-w25c-proposal-test-vm-identities-from-the-real-counter-DONE-20261015`):
/// a test identity is never 0, never a live real VM's, never another
/// test's, and a real VM's teardown cannot reach it.
#[test]
fn gcg_k2_test_vm_identities_come_from_the_real_counter() {
    let real = SharedVm::new(crate::config::VmConfig::default());
    let a = test_vm_identity();
    let b = test_vm_identity();
    assert_ne!(a, 0, "0 is \"no VM\"");
    assert_ne!(a, b, "each call is a new identity");
    assert_ne!(a, real.vm_identity);
    assert_ne!(b, real.vm_identity);
    assert!(
        a > real.vm_identity,
        "drawn after the real VM, from one counter"
    );
    let later = SharedVm::new(crate::config::VmConfig::default());
    assert!(
        later.vm_identity != a && later.vm_identity != b,
        "a later real VM never receives a test identity"
    );
}
