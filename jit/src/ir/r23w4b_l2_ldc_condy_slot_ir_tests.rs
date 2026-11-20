use super::*;

/// `Object f() { return <condy>; }` -- `ldc #1 ; areturn` -- with the
/// condy rows `slots` (`pc -> slot`).
fn build(slots: HashMap<usize, usize>) -> Option<Graph> {
    let code = [0x12, 0x01, 0xb0, 0, 0];
    let mut b = IrBuilder::new(0, 1);
    b.set_ldc_condy_slot_info(slots);
    b.build(&code, 3)
}

/// JIT r23 wave 4b, lane L2 (`CRATONVM_JIT_IR_LDC_CONDY_SLOT`): a resolved
/// reference condy is built as the read of its slot -- one
/// `Op::IndyRootRef`, no helper call, no `ConstClass` -- and with no row
/// the build is still refused (the method stays single-pass).
#[test]
fn a_slotted_condy_ldc_is_built_as_its_slot_read() {
    let g = build(HashMap::from([(0usize, 0x6100usize)])).expect("builds");
    assert_eq!(
        g.nodes
            .iter()
            .filter(|n| n.op == Op::IndyRootRef { slot_addr: 0x6100 })
            .count(),
        1
    );
    assert!(g
        .nodes
        .iter()
        .all(|n| !matches!(n.op, Op::Call { .. } | Op::ConstClass { .. })));
    assert!(build(HashMap::new()).is_none());
}
