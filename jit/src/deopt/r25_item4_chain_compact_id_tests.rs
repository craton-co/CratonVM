use super::*;

const CALLEE: &str = "T.plain:(LH;I)I";
const OUTER: &str = "T.main:([Ljava/lang/String;)V";

#[test]
fn chain_compact_id_is_per_scope_and_bci() {
    let a = chain_compact_speculation_id(CALLEE, 7);
    assert_eq!(a, chain_compact_speculation_id(CALLEE, 7));
    assert_ne!(a, SPECULATION_ID_ANY);
    assert_ne!(a, chain_compact_speculation_id(CALLEE, 8));
    assert_ne!(a, chain_compact_speculation_id("T.other:()V", 7));
    assert_ne!(a, speculation_id(7, DeoptReason::SpeculationFailed));
    assert_ne!(a, chain_predicate_speculation_id(CALLEE, 7, PredicateKind::NullBase));
    assert!(is_chain_compact_speculation_id(CALLEE, 7, a));
    assert!(!is_chain_compact_speculation_id(CALLEE, 8, a));
    assert!(!is_chain_compact_speculation_id("", 7, a));
}

#[test]
fn chain_compact_withdrawn_reads_its_own_id_or_the_wildcard() {
    let despec = DespecRegistry::new();
    assert!(!chain_compact_withdrawn(&despec, OUTER, 5, CALLEE, 7));
    despec.insert(OUTER, 5, chain_predicate_speculation_id(CALLEE, 7, PredicateKind::NullBase));
    assert!(
        !chain_compact_withdrawn(&despec, OUTER, 5, CALLEE, 7),
        "another named guard of the same splice leaves it standing"
    );
    despec.insert(OUTER, 5, chain_compact_speculation_id(CALLEE, 9));
    assert!(!chain_compact_withdrawn(&despec, OUTER, 5, CALLEE, 7));
    despec.insert(OUTER, 5, chain_compact_speculation_id(CALLEE, 7));
    assert!(chain_compact_withdrawn(&despec, OUTER, 5, CALLEE, 7));
    assert!(!chain_compact_withdrawn(&despec, OUTER, 6, CALLEE, 7));
    let wild = DespecRegistry::new();
    wild.insert(OUTER, 5, SPECULATION_ID_ANY);
    assert!(chain_compact_withdrawn(&wild, OUTER, 5, CALLEE, 7));
}
