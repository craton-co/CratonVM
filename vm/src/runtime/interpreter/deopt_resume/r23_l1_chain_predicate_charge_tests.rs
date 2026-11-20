use super::chain_predicate_charge_id;
use cratonvm_jit::deopt::{
    chain_predicate_speculation_id, speculation_id, DeoptCause, DeoptReason, PredicateKind,
    ReconstructedFrame, SPECULATION_ID_ANY,
};

const CALLEE: &str = "T.plain:(LH;I)I";

fn frame(chain: bool) -> ReconstructedFrame {
    ReconstructedFrame {
        class_id: 0,
        method_key: CALLEE.into(),
        bci: 10,
        caller_frames: if chain {
            vec![ReconstructedFrame {
                class_id: 0,
                method_key: "T.main:([Ljava/lang/String;)V".into(),
                bci: 5,
                ..Default::default()
            }]
        } else {
            Vec::new()
        },
        ..Default::default()
    }
}

fn cause(reason: DeoptReason, speculation_id: u32) -> Option<DeoptCause> {
    Some(DeoptCause {
        reason,
        speculation_id,
    })
}

#[test]
fn r23_l1_only_a_chain_predicates_own_id_is_named() {
    let ut = DeoptReason::UncommonTrap;
    let own = chain_predicate_speculation_id(CALLEE, 10, PredicateKind::NullBase);
    assert_eq!(chain_predicate_charge_id(&frame(true), cause(ut, own)), Some(own));
    // A flat frame, another bci's id, the plain or wildcard id, another
    // reason, no cause: the old attribution.
    assert_eq!(chain_predicate_charge_id(&frame(false), cause(ut, own)), None);
    let elsewhere = chain_predicate_speculation_id(CALLEE, 11, PredicateKind::NullBase);
    assert_eq!(chain_predicate_charge_id(&frame(true), cause(ut, elsewhere)), None);
    assert_eq!(
        chain_predicate_charge_id(&frame(true), cause(ut, speculation_id(10, ut))),
        None
    );
    assert_eq!(chain_predicate_charge_id(&frame(true), cause(ut, SPECULATION_ID_ANY)), None);
    assert_eq!(
        chain_predicate_charge_id(&frame(true), cause(DeoptReason::NullCheck, own)),
        None
    );
    assert_eq!(chain_predicate_charge_id(&frame(true), None), None);
}

/// JIT round 25 (item 4 residual 1): a spliced compactness guard's chain
/// trap is named by its own chain id under `SpeculationFailed`, and only
/// then; the combined reader answers either kind.
#[test]
fn r25_item4_a_spliced_compactness_guard_is_named_by_its_chain_id() {
    use super::{chain_compact_charge_id, chain_named_charge_id};
    use cratonvm_jit::deopt::chain_compact_speculation_id;
    let sf = DeoptReason::SpeculationFailed;
    let own = chain_compact_speculation_id(CALLEE, 10);
    assert_eq!(chain_compact_charge_id(&frame(true), cause(sf, own)), Some(own));
    assert_eq!(chain_named_charge_id(&frame(true), cause(sf, own)), Some(own));
    assert_eq!(chain_compact_charge_id(&frame(false), cause(sf, own)), None);
    let elsewhere = chain_compact_speculation_id(CALLEE, 11);
    assert_eq!(chain_compact_charge_id(&frame(true), cause(sf, elsewhere)), None);
    assert_eq!(
        chain_compact_charge_id(&frame(true), cause(sf, speculation_id(10, sf))),
        None,
        "the flat derived id is the wildcard path"
    );
    assert_eq!(
        chain_compact_charge_id(&frame(true), cause(DeoptReason::UncommonTrap, own)),
        None
    );
    let pred = chain_predicate_speculation_id(CALLEE, 10, PredicateKind::NullBase);
    assert_eq!(
        chain_named_charge_id(&frame(true), cause(DeoptReason::UncommonTrap, pred)),
        Some(pred)
    );
    assert_eq!(chain_named_charge_id(&frame(true), cause(sf, SPECULATION_ID_ANY)), None);
}
