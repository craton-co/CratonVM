use super::*;

// 0: invokedynamic #1 ()L  5: areturn
const CODE: [u8; 8] = [0xba, 0x00, 0x01, 0x00, 0x00, 0xb0, 0, 0];

fn build(rows: &[(usize, usize)]) -> Option<Graph> {
    let mut builder = IrBuilder::new(0, 1);
    builder.set_indy_trap_sites(HashMap::from([(0usize, (0usize, b'L'))]));
    builder.add_indy_bridge_sites(rows.iter().copied());
    builder.build(&CODE, 6)
}

#[test]
fn a_published_word_row_is_a_bridge_not_a_trap() {
    let g = build(&[(0, 0x5170)]).expect("builds");
    assert!(g
        .nodes
        .iter()
        .any(|n| matches!(n.op, Op::IndyBridge { site: 0x5170 })));
    assert!(g.site_trap_pcs.is_empty(), "no trap at the bridged site");
    // Without the row the arm is the old one: the trap.
    let trapped = build(&[]).expect("the trap keeps the method");
    assert_eq!(trapped.site_trap_pcs, vec![0]);
}

#[test]
fn an_added_row_never_replaces_a_recorded_one() {
    let mut builder = IrBuilder::new(0, 1);
    builder.set_indy_bridge_sites(HashMap::from([(0usize, 0x1111usize)]));
    builder.add_indy_bridge_sites([(0usize, 0x2222usize), (9, 0x3333)]);
    assert_eq!(builder.indy_bridge_sites.get(&0), Some(&0x1111));
    assert_eq!(builder.indy_bridge_sites.get(&9), Some(&0x3333));
}

