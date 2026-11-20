use super::*;

#[test]
fn a_phi_of_byte_array_reads_and_null_is_a_byte_array_base() {
    let mut g = Graph::empty(8);
    let merge = g.add(Op::Start, IrType::Control, vec![], None);
    let p0 = g.add(Op::Param(0), IrType::Ref, vec![], None);
    let load = Op::Load(MemKind::Ref);
    let hb1 = g.add(load.clone(), IrType::Ref, vec![merge, merge, p0], Some(1));
    let hb2 = g.add(load.clone(), IrType::Ref, vec![merge, merge, p0], Some(9));
    let null = g.add(Op::Const(0), IrType::Ref, vec![], None);
    let other = g.add(load, IrType::Ref, vec![merge, merge, p0], Some(17));
    g.byte_array_nodes.insert(hb1);
    g.byte_array_nodes.insert(hb2);
    let phi = g.add(Op::Phi, IrType::Ref, vec![merge, hb1, hb2], None);
    let loop_phi = g.add(Op::Phi, IrType::Ref, vec![merge, hb1, 0], None);
    // The loop φ's back edge is itself.
    g.nodes[loop_phi as usize].inputs[2] = loop_phi;
    let with_null = g.add(Op::Phi, IrType::Ref, vec![merge, phi, null], None);
    let mixed = g.add(Op::Phi, IrType::Ref, vec![merge, hb1, other], None);
    assert!(scoped_base_is_byte_array(&g, hb1));
    assert!(scoped_base_is_byte_array(&g, phi));
    assert!(scoped_base_is_byte_array(&g, loop_phi));
    assert!(scoped_base_is_byte_array(&g, with_null));
    assert!(!scoped_base_is_byte_array(&g, mixed), "one unproven input");
    assert!(!scoped_base_is_byte_array(&g, other));
    assert!(!scoped_base_is_byte_array(&g, p0));
}
