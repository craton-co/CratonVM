use super::chain_trap_charge;
use cratonvm_jit::deopt::{DeoptCause, DeoptReason, ReconstructedFrame, SPECULATION_ID_ANY};

fn chain(bci: u32) -> ReconstructedFrame {
    ReconstructedFrame {
        class_id: 0,
        method_key: "p/Callee.m:()V".into(),
        bci,
        caller_frames: vec![ReconstructedFrame {
            class_id: 0,
            method_key: "p/Outer.run:()V".into(),
            bci: 12,
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// nio21 wave 7 (L2): a chain is charged under the stash's reason with
/// the wildcard (the doors' grain), `UncommonTrap` without a cause; a
/// single frame and the sentinel keep the service's own attribution.
#[test]
fn nio21w7_a_chain_trap_is_charged_under_the_stash_reason_with_the_wildcard() {
    let cause = DeoptCause {
        reason: DeoptReason::ReceiverTypeChanged,
        speculation_id: 77,
    };
    assert_eq!(
        chain_trap_charge(&chain(3), Some(cause)),
        Some(DeoptCause {
            reason: DeoptReason::ReceiverTypeChanged,
            speculation_id: SPECULATION_ID_ANY,
        })
    );
    assert_eq!(
        chain_trap_charge(&chain(3), None).map(|c| c.reason),
        Some(DeoptReason::UncommonTrap)
    );
    assert_eq!(chain_trap_charge(&chain(u32::MAX), Some(cause)), None);
    let single = ReconstructedFrame {
        class_id: 0,
        method_key: "p/Outer.run:()V".into(),
        bci: 3,
        ..Default::default()
    };
    assert_eq!(chain_trap_charge(&single, Some(cause)), None);    }