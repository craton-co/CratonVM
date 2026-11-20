use super::*;
use std::collections::{HashMap, HashSet};

/// `static int f(int i) { try { return g(i); } catch (..) { return 2; } }`:
/// the call at pc 1 is covered by `[0, 5)`, the handler at 5.
const ONE_HANDLER: [u8; 8] = [
    0x1a, // 0: iload_0
    0xb8, 0x00, 0x01, // 1: invokestatic #1  (I)I
    0xac, // 4: ireturn
    0x4c, // 5: astore_1 (handler)
    0x05, // 6: iconst_2
    0xac, // 7: ireturn
];

/// The same call under two rows, handlers at 5 and 8 (the `ite` shape:
/// two `catch` clauses of one `try`).
const TWO_HANDLERS: [u8; 11] = [
    0x1a, // 0: iload_0
    0xb8, 0x00, 0x01, // 1: invokestatic #1  (I)I
    0xac, // 4: ireturn
    0x4c, // 5: astore_1 (handler 0)
    0x04, // 6: iconst_1
    0xac, // 7: ireturn
    0x4c, // 8: astore_1 (handler 1)
    0x05, // 9: iconst_2
    0xac, // 10: ireturn
];

fn row(handler_pc: usize, catch: IrCatchClass) -> IrLocalThrowRow {
    IrLocalThrowRow {
        start_pc: 0,
        end_pc: 5,
        handler_pc,
        catch,
    }
}

/// A leaked static `(I)I` call row, as the other builder tests make them.
fn call_row() -> usize {
    let info: &'static crate::JitInvokeInfo = Box::leak(Box::new(crate::JitInvokeInfo {
        class_name: "p/Host",
        method_name: "g",
        descriptor: "(I)I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 3,
        declaring_class_id: 0x7101,
        owner_class_id: 0,
    }));
    info as *const crate::JitInvokeInfo as usize
}

fn build(code: &[u8], rows: Vec<IrLocalThrowRow>, names: Option<Vec<&'static str>>) -> Graph {
    let mut b = IrBuilder::new(1, 2);
    b.set_param_types(&[IrType::Int]);
    b.set_invoke_info(HashMap::from([(1usize, (call_row(), 1usize, b'I'))]));
    b.set_local_throw_rows(rows);
    if let Some(names) = names {
        b.set_call_throw_catch_names(names, 0x7101);
    }
    b.build(code, code.len())
        .expect("the method builds with and without the edges")
}

fn count(g: &Graph, pred: impl Fn(&Op) -> bool) -> usize {
    g.nodes.iter().filter(|n| pred(&n.op)).count()
}

/// One catch-all row: the call grows one flag, one `If` and one caught
/// throwable, the handler is compiled (a second `Return`), and the site
/// row names the handler with the empty catch-all name.
#[test]
fn a_call_inside_a_try_branches_to_its_handler() {
    let g = build(
        &ONE_HANDLER,
        vec![row(5, IrCatchClass::Any)],
        Some(vec![""]),
    );
    assert_eq!(count(&g, |op| matches!(op, Op::CallThrew { .. })), 1);
    assert_eq!(count(&g, |op| *op == Op::CaughtException), 1);
    assert_eq!(count(&g, |op| *op == Op::Return), 2);
    assert_eq!(count(&g, |op| *op == Op::Throw), 0);
    assert_eq!(
        g.call_throw_sites,
        vec![IrCallThrowSite {
            bci: 1,
            declaring_class_id: 0x7101,
            candidates: vec![("", 5)],
            spliced: false,
        }]
    );
    // The flag reads the call (an ordering edge) and its `If` reads it.
    let (flag, node) = g
        .nodes
        .iter()
        .enumerate()
        .find(|(_, n)| matches!(n.op, Op::CallThrew { site: 0, index: 0 }))
        .expect("the flag");
    assert!(matches!(
        g.nodes[node.inputs[1] as usize].op,
        Op::Call { .. }
    ));
    assert!(g
        .nodes
        .iter()
        .any(|n| n.op == Op::If && n.inputs.get(1).copied() == Some(flag as NodeId)));
    // The handler's frame: the JVMS 2.10 stack `[exception]`.
    assert_eq!(
        g.safepoints
            .iter()
            .find(|s| s.bci == 5)
            .map(|s| s.stack.len()),
        Some(1)
    );
    crate::ir_verify::verify_graph_at_phase(&g, crate::ir_verify::PHASE_POST_BUILD)
        .expect("the edge verifies");
}

/// Two rows: one flag per candidate, in table order, chained on the
/// false edges, each true edge into its own handler.
#[test]
fn every_covering_row_gets_its_own_edge_in_table_order() {
    let g = build(
        &TWO_HANDLERS,
        vec![
            row(5, IrCatchClass::Class(9)),
            row(8, IrCatchClass::Unknown),
        ],
        Some(vec!["p/A", "p/B"]),
    );
    assert_eq!(
        count(&g, |op| matches!(op, Op::CallThrew { index: 0, .. })),
        1
    );
    assert_eq!(
        count(&g, |op| matches!(op, Op::CallThrew { index: 1, .. })),
        1
    );
    assert_eq!(count(&g, |op| *op == Op::CaughtException), 2);
    assert_eq!(count(&g, |op| *op == Op::Return), 3);
    assert_eq!(
        g.call_throw_sites[0].candidates,
        vec![("p/A", 5), ("p/B", 8)]
    );
    crate::ir_verify::verify_graph_at_phase(&g, crate::ir_verify::PHASE_POST_BUILD)
        .expect("the edges verify");
}

/// No names (the switch off in `lib.rs`), or a name list that does not
/// match the rows: exactly the wave-2 graph -- no flag, the handler not
/// compiled.
#[test]
fn without_the_names_the_call_keeps_its_exit() {
    for names in [None, Some(vec!["", ""])] {
        let g = build(&ONE_HANDLER, vec![row(5, IrCatchClass::Any)], names.clone());
        assert_eq!(
            count(&g, |op| matches!(op, Op::CallThrew { .. })),
            0,
            "{names:?}"
        );
        assert_eq!(count(&g, |op| *op == Op::CaughtException), 0, "{names:?}");
        assert_eq!(count(&g, |op| *op == Op::Return), 1, "{names:?}");
        assert!(g.call_throw_sites.is_empty(), "{names:?}");
    }
}

/// The planner is all-or-nothing per site: one covering row whose handler
/// is at or before the call, or more rows than a pad tests, drops the
/// whole site.
#[test]
fn a_site_with_a_backward_handler_or_too_many_rows_is_not_planned() {
    let verified = cratonvm_reader::verified_code(&ONE_HANDLER).expect("decodes");
    let reachable: HashSet<usize> = [0usize, 1, 4].into_iter().collect();
    let plan = |rows: &[IrLocalThrowRow]| {
        call_site_throw_plan(&verified, &reachable, &ONE_HANDLER, 8, rows)
    };
    assert_eq!(
        plan(&[row(5, IrCatchClass::Any)]),
        HashMap::from([(1usize, vec![5usize])])
    );
    assert!(plan(&[row(5, IrCatchClass::Any), row(1, IrCatchClass::Any)]).is_empty());
    let many: Vec<IrLocalThrowRow> = (0..=IR_CALL_THROW_MAX_CANDIDATES)
        .map(|_| row(5, IrCatchClass::Any))
        .collect();
    assert!(plan(&many).is_empty());
    // A row that does not cover the call plans nothing.
    assert!(plan(&[IrLocalThrowRow {
        start_pc: 4,
        end_pc: 5,
        handler_pc: 5,
        catch: IrCatchClass::Any,
    }])
    .is_empty());
}
