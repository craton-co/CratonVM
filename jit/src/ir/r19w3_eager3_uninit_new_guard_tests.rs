use super::*;

/// `new #1; areturn` (a builder fixture, not verifiable bytecode).
const CODE: [u8; 6] = [0xbb, 0x00, 0x01, 0xb0, 0, 0];
const WORD: usize = 0x1000;

fn build(row: bool, guard_word: usize) -> Option<Graph> {
    let mut b = IrBuilder::new(0, 0);
    if row {
        b.set_new_info(HashMap::from([(0usize, (7u32, 0usize))]), HashSet::new());
    }
    b.set_uninit_new_guard_sites(HashMap::from([(0usize, (guard_word, 2u8))]));
    b.build(&CODE, 4)
}

#[test]
fn an_admitted_site_guards_the_resolved_allocation() {
    let g = build(true, WORD).expect("the guarded `new` keeps the method");
    let test = g
        .nodes
        .iter()
        .position(|n| {
            matches!(
                n.op,
                Op::ClassInitTest {
                    state_addr: WORD,
                    initialized: 2
                }
            )
        })
        .expect("the init word is tested");
    assert!(
        g.nodes.iter().any(|n| matches!(n.op, Op::Guard { bci: 0 })
            && n.inputs.get(1).copied() == NodeId::try_from(test).ok()),
        "a guard at the `new` reads the test"
    );
    assert!(
        g.nodes
            .iter()
            .any(|n| matches!(n.op, Op::New { class_id: 7, .. })),
        "the passing edge allocates"
    );
    assert_eq!(g.trap_cause_at(0), Some(TrapCause::UninitNew));
    assert!(g.site_trap_pcs.is_empty(), "a guard is not a site trap");
}

#[test]
fn a_refused_guard_takes_the_row_less_path() {
    // A zero word is refused; the row is then not used, so the method
    // takes today's row-less path (the bail, unless a trap opt-in is on).
    if ir_unresolved_class_trap_enabled() {
        return;
    }
    assert!(build(true, 0).is_none(), "no guard, no row: the bail");
}

#[test]
fn the_trap_cause_round_trips_its_code() {
    for c in [
        TrapCause::Indy,
        TrapCause::UnresolvedTypeCheck,
        TrapCause::UnresolvedNew,
        TrapCause::UninitNew,
    ] {
        assert_eq!(TrapCause::from_code(c.code()), Some(c));
    }
    assert_eq!(TrapCause::from_code(9), None);
}

#[test]
fn the_class_init_test_is_pinned_and_impure() {
    let op = Op::ClassInitTest {
        state_addr: WORD,
        initialized: 2,
    };
    assert!(!op.is_pure(), "the word changes while the body runs");
    assert!(!may_raise(&op));
    assert!(matches!(memory_shape_of(&op), MemoryShape::None));
}
