use super::*;

// 0: invokedynamic #1 ()L  5: areturn -- nothing committed, so the
// default arm would plant its trap.
const CODE: [u8; 8] = [0xba, 0x00, 0x01, 0x00, 0x00, 0xb0, 0, 0];

fn build(published_word: bool) -> Option<Graph> {
    let mut builder = IrBuilder::new(0, 1);
    builder.set_indy_trap_sites(HashMap::from([(0usize, (0usize, b'L'))]));
    // What the refusal-only door block records for every resolved site.
    builder.set_indy_bridge_sites(HashMap::from([(0usize, 0x5170usize)]));
    builder.set_indy_bridge_refusal_only(true, true);
    if published_word {
        builder.add_indy_bridge_sites([(0usize, 0x5170usize)]);
    }
    builder.build(&CODE, 6)
}

fn bridges(g: &Graph) -> usize {
    g.nodes
        .iter()
        .filter(|n| matches!(n.op, Op::IndyBridge { site: 0x5170 }))
        .count()
}


