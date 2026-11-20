use super::*;

/// `new #1; areturn` (a builder fixture, not verifiable bytecode): the
/// `new` at pc 0 has no `new_info` row, as a `Deferred` site has none.
const CODE: [u8; 6] = [0xbb, 0x00, 0x01, 0xb0, 0, 0];

fn build(admitted: bool) -> Option<Graph> {
    let mut b = IrBuilder::new(0, 0);
    if admitted {
        b.set_uninit_new_trap_pcs(HashSet::from([0usize]));
    }
    b.build(&CODE, 4)
}

#[test]
fn an_admitted_uninit_new_builds_a_guard_at_its_bci() {
    if !ir_site_trap_enabled() {
        return;
    }
    let g = build(true).expect("the trap keeps the method");
    assert!(
        g.nodes.iter().any(|n| matches!(n.op, Op::Guard { bci: 0 })),
        "the `new` at pc 0 is an uncommon trap"
    );
    assert!(
        !g.nodes.iter().any(|n| matches!(n.op, Op::New { .. })),
        "no allocation is built for the trapped site"
    );
}

#[test]
fn a_site_not_admitted_keeps_todays_path() {
    // Today's path: the unresolved-class trap when that opt-in is on,
    // else the bail to single-pass.
    if ir_unresolved_class_trap_enabled() {
        return;
    }
    assert!(build(false).is_none(), "the deferred `new` bails as before");
}

#[test]
fn the_cause_has_its_own_census_row() {
    assert_eq!(TrapCause::UninitNew.as_str(), "uninit-new");
    assert!(ir_trap_census().iter().any(|(c, _)| *c == "uninit-new"));
    assert!(ir_trap_refusal_census()
        .iter()
        .any(|(c, _)| *c == "uninit-new"));
    assert_ne!(
        TrapCause::UninitNew.index(),
        TrapCause::UnresolvedNew.index()
    );
}
