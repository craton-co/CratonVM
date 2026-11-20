//! nio21 wave 7 (lane L4, W5L4-3): `Graph::byte_array_nodes`.
use super::*;

fn build(byte_array_pcs: &[usize], finals: &[usize]) -> Graph {
    cratonvm_types::flags::with_thread_overrides(
        &[],
        || {
            let mut b = IrBuilder::new(2, 2);
            b.set_param_types(&[IrType::Ref, IrType::Int]);
            b.set_field_info([1usize, 5].iter().map(|&pc| (pc, (0usize, b'I'))).collect());
            b.add_final_field_pcs(finals.iter().copied());
            b.add_byte_array_field_pcs(byte_array_pcs.iter().copied());
            // 0 aload_0; 1 getfield #2; 4 aload_0; 5 getfield #2; 8 iadd; 9 ireturn
            let code: [u8; 12] = [
                0x2a, 0xb4, 0x00, 0x02, 0x2a, 0xb4, 0x00, 0x02, 0x60, 0xac, 0, 0,
            ];
            b.build(&code, 10).expect("IR build")
        },
    )
}

#[test]
fn a_listed_read_and_its_forwarded_twin_are_byte_array_nodes() {
    let g = build(&[1], &[]);
    assert_eq!(
        g.byte_array_nodes.len(),
        1,
        "only the read at the listed pc"
    );
    let n = *g.byte_array_nodes.iter().next().unwrap();
    assert!(matches!(g.nodes[n as usize].op, Op::Load(_)));
    assert_eq!(g.nodes[n as usize].bytecode_pc, Some(1));
    // The second read forwarded from the first is the same node.
    let fwd = build(&[1, 5], &[1, 5]);
    assert_eq!(fwd.byte_array_nodes.len(), 1);
}

#[test]
fn no_listed_pc_claims_nothing() {
    assert!(build(&[], &[]).byte_array_nodes.is_empty());
    assert!(build(&[], &[1, 5]).byte_array_nodes.is_empty());
}

