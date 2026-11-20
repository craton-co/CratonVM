use super::*;

#[test]
fn selected_arithmetic_names_its_checked_array_address() {
    for (load_opcode, alu_opcode, ret_opcode, lhs_opcode, ty, scale) in [
        (0x2e, 0x60, 0xac, 0x1c, IrType::Int, 4),
        (0x2f, 0x65, 0xad, 0x20, IrType::Long, 8),
    ] {
        // An explicit null and length guard, then left + a[i] or left - a[i].
        let bytecode = [
            0x2a, 0xc6, 0, 13, 0x1b, 0x9b, 0, 9, 0x1b, 0x2a, 0xbe,
            0xa1, 0, 5, lhs_opcode, ret_opcode, lhs_opcode, 0x2a, 0x1b,
            load_opcode, alu_opcode, ret_opcode,
        ];
        let mut builder = crate::ir::IrBuilder::new(4, 4);
        builder.set_param_types(&[IrType::Ref, IrType::Int, ty]);
        let mut graph = builder.build(&bytecode, bytecode.len()).unwrap();
        crate::ea_ir_bridge::ir_prune_unconsumable_snapshots(&mut graph, &[]);
        let schedule = crate::ir_schedule::schedule(&graph);
        let checks = crate::ir_check_elim::analyze(&graph, &schedule);
        let (load, node) = graph.nodes.iter().enumerate()
            .find(|(_, n)| matches!(n.op, IrOp::ArrayLoad(_))).unwrap();
        let load = load as NodeId;
        let addr = checked_integer_array_address(&graph, &checks, load)
            .expect("array read has both dominating checks");
        assert_eq!(addr.base, Some(node.inputs[2]));
        assert_eq!(addr.index, Some(node.inputs[3]));
        assert_eq!(addr.scale, scale);
        let opts = SelectOptions { fold_loads: true, ..SelectOptions::default() };
        let mut selector = MethodSelector::new(&graph);
        selector.allow_checked_array_reads(&checks);
        let found = schedule.blocks.iter().any(|block| {
            let selection = selector.select_block(&block.nodes, block.terminator, &opts);
            selection.tiles.iter()
                .any(|tile| {
                    tile.rule == Rule::AluFoldedLoad
                        && tile.covered.contains(&load)
                        && tile.insts.iter().any(|inst| {
                            matches!(inst, MInst::AluRM {
                                addr: AddrSource::Expr(bound), load: read, ..
                            } if *bound == addr && *read == load)
                        })
                })
        });
        assert!(found, "{ty:?}: selector kept an intermediate array load");
    }
}
