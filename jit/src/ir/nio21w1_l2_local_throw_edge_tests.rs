use super::*;
use std::collections::{HashMap, HashSet};

/// `static int f(int i)`: `new #1; athrow` inside `[0, 4)`, handler at 4
/// (`astore_1; iload_0; ireturn`) -- the `throwlocal` row's shape with
/// the constructor elided.
const THROW_LOCAL: [u8; 7] = [
    0xbb, 0x00, 0x01, // 0: new #1 (class 7)
    0xbf, // 3: athrow
    0x4c, // 4: astore_1 (handler)
    0x1a, // 5: iload_0
    0xac, // 6: ireturn
];

fn row(catch: IrCatchClass) -> IrLocalThrowRow {
    IrLocalThrowRow {
        start_pc: 0,
        end_pc: 4,
        handler_pc: 4,
        catch,
    }
}

fn build_throw_local(rows: Vec<IrLocalThrowRow>) -> Graph {
    let mut b = IrBuilder::new(1, 2);
    b.set_param_types(&[IrType::Int]);
    b.set_new_info(HashMap::from([(0usize, (7u32, 0usize))]), HashSet::new());
    b.set_local_throw_rows(rows);
    b.build(&THROW_LOCAL, THROW_LOCAL.len())
        .expect("the method builds with and without the edge")
}

fn count(g: &Graph, pred: impl Fn(&Op) -> bool) -> usize {
    g.nodes.iter().filter(|n| pred(&n.op)).count()
}

fn handler_snapshot_stack_depth(g: &Graph) -> Option<usize> {
    g.safepoints
        .iter()
        .find(|s| s.bci == 4)
        .map(|s| s.stack.len())
}

/// The exact class's own row: the throw is a branch, the handler is
/// compiled with the JVMS `[exception]` stack, and nothing leaves.
#[test]
fn a_throw_its_own_catch_takes_is_a_branch_to_the_handler() {
    let g = build_throw_local(vec![row(IrCatchClass::Class(7))]);
    assert_eq!(count(&g, |op| *op == Op::Throw), 0);
    assert_eq!(count(&g, |op| *op == Op::Return), 1);
    assert_eq!(handler_snapshot_stack_depth(&g), Some(1));
}

/// A catch-all row takes every throwable.
#[test]
fn a_catch_all_row_takes_it_too() {
    let g = build_throw_local(vec![row(IrCatchClass::Any)]);
    assert_eq!(count(&g, |op| *op == Op::Throw), 0);
    assert_eq!(handler_snapshot_stack_depth(&g), Some(1));
}

/// Another class (a subclass relation is not decided here), an unknown
/// first row, or no rows at all: the throw leaves as before and the
/// handler is not in the graph.
#[test]
fn an_undecided_throw_leaves_the_frame_as_before() {
    for rows in [
        vec![row(IrCatchClass::Class(8))],
        vec![row(IrCatchClass::Unknown), row(IrCatchClass::Class(7))],
        Vec::new(),
    ] {
        let g = build_throw_local(rows.clone());
        assert_eq!(count(&g, |op| *op == Op::Throw), 1, "{rows:?}");
        assert_eq!(count(&g, |op| *op == Op::Return), 0, "{rows:?}");
        assert_eq!(handler_snapshot_stack_depth(&g), None, "{rows:?}");
        assert!(
            !g.nodes
                .iter()
                .any(|n| n.op == Op::Merge && n.inputs.is_empty()),
            "{rows:?}: no orphan merge survives"
        );
    }
}

/// A possibly null parameter takes the catch-all edge only under a non-null
/// guard, with null replayed at the original `athrow`.
#[test]
fn a_rethrown_value_uses_a_non_null_catch_all_guard() {
    // aload_0; athrow; astore_1 (handler); iconst_1; ireturn
    let code = [0x2a, 0xbf, 0x4c, 0x04, 0xac];
    let mut b = IrBuilder::new(1, 2);
    b.set_param_types(&[IrType::Ref]);
    b.set_local_throw_rows(vec![IrLocalThrowRow {
        start_pc: 0,
        end_pc: 2,
        handler_pc: 2,
        catch: IrCatchClass::Any,
    }]);
    let g = b.build(&code, code.len()).expect("builds");
    assert_eq!(count(&g, |op| matches!(op, Op::Cmp(CmpOp::Ne))), 1);
    assert_eq!(count(&g, |op| *op == Op::Throw), 1);
    assert_eq!(count(&g, |op| *op == Op::Return), 1);
    assert_eq!(handler_snapshot_stack_depth(&g), Some(1));
}

/// An unknown throwable's runtime type selects the only typed local row;
/// a miss keeps a replay exit at `athrow`.
#[test]
fn a_runtime_subtype_test_enters_a_typed_handler() {
    // aload_0; athrow; astore_1; iconst_1; ireturn
    let code = [0x2a, 0xbf, 0x4c, 0x04, 0xac];
    let mut b = IrBuilder::new(1, 2);
    b.set_param_types(&[IrType::Ref]);
    b.set_local_throw_rows(vec![IrLocalThrowRow {
        start_pc: 0,
        end_pc: 2,
        handler_pc: 2,
        catch: IrCatchClass::Class(8),
    }]);
    b.set_local_throw_catch_targets(vec![Some((0x1234, 12))]);
    let g = b.build(&code, code.len()).expect("builds");
    assert_eq!(count(&g, |op| matches!(op, Op::InstanceOf { .. })), 1);
    assert_eq!(count(&g, |op| *op == Op::Throw), 1);
    assert_eq!(handler_snapshot_stack_depth(&g), Some(1));
}

/// The candidate scan reads the FIRST covering row only, and only a
/// handler after its `athrow`.
#[test]
fn candidates_follow_the_first_covering_row() {
    let verified = cratonvm_reader::verified_code(&THROW_LOCAL).expect("decodes");
    let reachable: HashSet<usize> = [0usize, 3].into_iter().collect();
    let cand = |rows: &[IrLocalThrowRow]| {
        local_throw_candidate_handlers(&verified, &reachable, &THROW_LOCAL, 7, rows)
    };
    assert_eq!(
        cand(&[row(IrCatchClass::Class(9))]),
        HashSet::from([4usize])
    );
    assert!(cand(&[row(IrCatchClass::Unknown), row(IrCatchClass::Any)]).is_empty());
    assert!(cand(&[IrLocalThrowRow {
        start_pc: 0,
        end_pc: 3,
        handler_pc: 4,
        catch: IrCatchClass::Any,
    }])
    .is_empty());
}
