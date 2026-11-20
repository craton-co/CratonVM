use super::*;
use std::collections::{HashMap, HashSet};

/// `static int f(int i)`: `new #1 (class 7); athrow` inside `[0, 4)`; two
/// handlers: 4 (`astore_1; iconst_1; ireturn`) and 7 (`astore_1;
/// iconst_2; ireturn`).
const THROW_TWO: [u8; 10] = [
    0xbb, 0x00, 0x01, // 0: new #1 (class 7)
    0xbf, // 3: athrow
    0x4c, // 4: astore_1 (handler A)
    0x04, // 5: iconst_1
    0xac, // 6: ireturn
    0x4c, // 7: astore_1 (handler B)
    0x05, // 8: iconst_2
    0xac, // 9: ireturn
];

fn row(handler_pc: usize, catch: IrCatchClass) -> IrLocalThrowRow {
    IrLocalThrowRow {
        start_pc: 0,
        end_pc: 4,
        handler_pc,
        catch,
    }
}

fn build(rows: Vec<IrLocalThrowRow>, facts: &[((u32, u32), bool)]) -> Graph {
    let mut b = IrBuilder::new(1, 2);
    b.set_param_types(&[IrType::Int]);
    b.set_new_info(HashMap::from([(0usize, (7u32, 0usize))]), HashSet::new());
    b.set_local_throw_subclass_facts(facts.iter().copied().collect());
    b.set_local_throw_rows(rows);
    b.build(&THROW_TWO, THROW_TWO.len())
        .expect("the method builds with and without the edge")
}

fn throws(g: &Graph) -> usize {
    g.nodes.iter().filter(|n| n.op == Op::Throw).count()
}

fn entered(g: &Graph, handler: usize) -> bool {
    g.safepoints
        .iter()
        .any(|s| s.bci == handler && s.stack.len() == 1)
}

/// `catch (Super)` around `throw new Sub()`: the VM's `true` makes the
/// first row the target.
#[test]
fn a_superclass_row_the_vm_confirms_takes_the_throw() {
    let g = build(vec![row(4, IrCatchClass::Class(8))], &[((7, 8), true)]);
    assert_eq!(throws(&g), 0);
    assert!(entered(&g, 4));
}

/// A first row the VM rules out is skipped: the second row (a confirmed
/// superclass, then the exact class, then a catch-all) is the target.
#[test]
fn a_row_the_vm_rules_out_is_skipped_in_table_order() {
    for (second, facts) in [
        (
            IrCatchClass::Class(9),
            vec![((7, 8), false), ((7, 9), true)],
        ),
        (IrCatchClass::Class(7), vec![((7, 8), false)]),
        (IrCatchClass::Any, vec![((7, 8), false)]),
    ] {
        let g = build(vec![row(4, IrCatchClass::Class(8)), row(7, second)], &facts);
        assert_eq!(throws(&g), 0, "{second:?}");
        assert!(entered(&g, 7), "{second:?}");
        assert!(!entered(&g, 4), "{second:?}");
    }
}

/// An unanswered pair, an `Unknown` row, or a `false` with nothing after
/// it stops the decision: the throw leaves as before. A `true` FIRST row
/// wins over a later exact row (first match, not best match).
#[test]
fn an_undecided_row_stops_the_decision() {
    for (rows, facts) in [
        (
            vec![
                row(4, IrCatchClass::Class(8)),
                row(7, IrCatchClass::Class(7)),
            ],
            vec![],
        ),
        (
            vec![
                row(4, IrCatchClass::Unknown),
                row(7, IrCatchClass::Class(7)),
            ],
            vec![((7, 8), true)],
        ),
        (vec![row(4, IrCatchClass::Class(8))], vec![((7, 8), false)]),
        (
            vec![
                row(4, IrCatchClass::Class(8)),
                row(7, IrCatchClass::Class(9)),
            ],
            vec![((7, 8), false)],
        ),
    ] {
        let g = build(rows.clone(), &facts);
        assert_eq!(throws(&g), 1, "{rows:?} {facts:?}");
    }
    let g = build(
        vec![
            row(4, IrCatchClass::Class(8)),
            row(7, IrCatchClass::Class(7)),
        ],
        &[((7, 8), true)],
    );
    assert_eq!(throws(&g), 0);
    assert!(entered(&g, 4));
    assert!(!entered(&g, 7));
}

/// The later-row scan stops before an `Unknown` row and after a
/// catch-all.
#[test]
fn later_row_handlers_stop_at_unknown_and_catch_all() {
    let verified = cratonvm_reader::verified_code(&THROW_TWO).expect("decodes");
    let reachable: HashSet<usize> = [0usize, 3].into_iter().collect();
    let scan = |rows: &[IrLocalThrowRow]| {
        local_throw_later_row_handlers(&verified, &reachable, &THROW_TWO, 10, rows)
    };
    assert_eq!(
        scan(&[
            row(4, IrCatchClass::Class(8)),
            row(7, IrCatchClass::Class(9))
        ]),
        HashSet::from([4usize, 7])
    );
    assert_eq!(
        scan(&[
            row(4, IrCatchClass::Class(8)),
            row(7, IrCatchClass::Unknown)
        ]),
        HashSet::from([4usize])
    );
    assert_eq!(
        scan(&[row(4, IrCatchClass::Any), row(7, IrCatchClass::Class(9))]),
        HashSet::from([4usize])
    );
}
