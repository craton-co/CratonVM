use super::sp_intrinsic_trap_withdrawn_in;
use crate::deopt::{speculation_id, DeoptReason, DespecRegistry, SPECULATION_ID_ANY};

const KEY: &str = "H8Msgs.k:(II)Ljava/lang/Object;";

#[test]
fn a_receiver_trap_record_withdraws_the_intrinsic_at_its_bci_only() {
    let registry = DespecRegistry::new();
    assert!(!sp_intrinsic_trap_withdrawn_in(&registry, KEY, 160));
    registry.insert(KEY, 160, speculation_id(160, DeoptReason::ReceiverTypeChanged));
    assert!(sp_intrinsic_trap_withdrawn_in(&registry, KEY, 160));
    assert!(!sp_intrinsic_trap_withdrawn_in(&registry, KEY, 516), "another bci keeps it");
    assert!(
        !sp_intrinsic_trap_withdrawn_in(&registry, "H8Msgs.other:()V", 160),
        "another method keeps it"
    );
}

#[test]
fn the_wildcard_withdraws_it_and_another_speculation_does_not() {
    let registry = DespecRegistry::new();
    registry.insert(KEY, 516, speculation_id(516, DeoptReason::BoundsCheck));
    assert!(
        !sp_intrinsic_trap_withdrawn_in(&registry, KEY, 516),
        "a bounds-check record at the bci is about a different speculation"
    );
    registry.insert(KEY, 516, SPECULATION_ID_ANY);
    assert!(sp_intrinsic_trap_withdrawn_in(&registry, KEY, 516));
}
