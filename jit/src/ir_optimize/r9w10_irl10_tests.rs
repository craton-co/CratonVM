use super::*;
use crate::ir::IrBuilder;

/// One verdict per single-back-edge header: `Some(trip)` for a counted
/// loop, `Some(-1)` for a runtime-bound one, `None` for "not counted".
fn verdicts(graph: &mut Graph) -> Vec<Option<i64>> {
    collapse_trivial_merges(graph);
    let mut out = Vec::new();
    for region in 0..graph.nodes.len() as NodeId {
        if !matches!(graph.nodes[region as usize].op, Op::Region | Op::Merge)
            || graph.nodes[region as usize].inputs.len() != 2
        {
            continue;
        }
        let users = build_users(graph);
        let reach = forward_control_closure(graph, &users, region);
        let back: Vec<NodeId> = graph.nodes[region as usize]
            .inputs
            .iter()
            .copied()
            .filter(|&i| i != NO_NODE && reach.contains(&i))
            .collect();
        if back.len() != 1 {
            continue;
        }
        out.push(match analyze_counted_loop(graph, region, back[0]) {
            Ok(c) => Some(c.trip),
            Err(NotCounted::RuntimeBound(_)) => Some(-1),
            Err(_) => None,
        });
    }
    out
}

#[test]
fn a_long_lcmp_loop_is_a_counted_loop() {
    // static long h() { long s = 0; for (long i = 0; i < 4; i++) s += i; return s; }
    // (`4L` as `iconst_4; i2l`.)
    let const_bound = [
        0x09, 0x3F, 0x09, 0x41, // s = 0; i = 0
        0x20, 0x07, 0x85, 0x94, 0x9C, 0x00, 0x0E, // 4: i ? 4L; ifge 22
        0x1E, 0x20, 0x61, 0x3F, // 11: s += i
        0x20, 0x0A, 0x61, 0x41, // 15: i += 1
        0xA7, 0xFF, 0xF1, // 19: goto 4
        0x1E, 0xAD, // 22: lload_0; lreturn
        0, 0,
    ];
    let mut g = IrBuilder::new(0, 4)
        .build(&const_bound, 24)
        .expect("the constant-bound long loop builds");
    let v = verdicts(&mut g);
    assert_eq!(v.len(), 1, "one loop header: {v:?}");
    assert!(
        v[0] == Some(4) || v[0] == Some(-1),
        "the lcmp loop must be recognised as counted (trip 4, or runtime-bound \
         if the builder kept `i2l` unfolded), got {v:?}"
    );

    // static long g(long n) { long s = 0; for (long i = 0; i < n; i++) s += i * 3L + (i >> 1); return s; }
    let runtime_bound = [
        0x09, 0x41, 0x09, 0x37, 0x04, // s = 0; i = 0
        0x16, 0x04, 0x1E, 0x94, 0x9C, 0x00, 0x19, // 5: i ? n; ifge 34
        0x20, 0x16, 0x04, 0x06, 0x85, 0x69, // 12: s, i * 3L
        0x16, 0x04, 0x04, 0x7B, // 18: i >> 1
        0x61, 0x61, 0x41, // 22: + ; + ; lstore_2
        0x16, 0x04, 0x0A, 0x61, 0x37, 0x04, // 25: i = i + 1
        0xA7, 0xFF, 0xE6, // 31: goto 5
        0x20, 0xAD, // 34: lload_2; lreturn
        0, 0,
    ];
    let mut b = IrBuilder::new(1, 6);
    b.set_param_types(&[IrType::Long]);
    let mut g = b
        .build(&runtime_bound, 36)
        .expect("the runtime-bound long loop builds");
    assert_eq!(verdicts(&mut g), vec![Some(-1)]);
}

#[test]
fn the_lcmp_read_through_is_exact_and_oriented() {
    let mut g = Graph::empty(0);
    g.exit = 0;
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let a = g.add(Op::Param(0), IrType::Long, vec![], None);
    let b = g.add(Op::Param(1), IrType::Long, vec![], None);
    let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
    let one = g.add(Op::Const(1), IrType::Int, vec![], None);
    let l = g.add(Op::LCmp, IrType::Int, vec![a, b], None);
    assert_eq!(lcmp_operands_of_zero_test(&g, l, zero), Some([a, b]));
    // `0 cc lcmp(a, b)` is `b cc a`.
    assert_eq!(lcmp_operands_of_zero_test(&g, zero, l), Some([b, a]));
    // Against anything but zero it is not a plain compare.
    assert_eq!(lcmp_operands_of_zero_test(&g, l, one), None);
    assert_eq!(lcmp_operands_of_zero_test(&g, a, b), None);
    // An FCmp is never read through (NaN bias).
    let fa = g.add(Op::Param(2), IrType::Double, vec![], None);
    let fb = g.add(Op::Param(3), IrType::Double, vec![], None);
    let f = g.add(
        Op::FCmp {
            double: true,
            nan_greater: false,
        },
        IrType::Int,
        vec![fa, fb],
        None,
    );
    assert_eq!(lcmp_operands_of_zero_test(&g, f, zero), None);
    // And the equivalence itself, over the orientations the builder emits.
    for (x, y) in [
        (0i64, 0i64),
        (1, 2),
        (2, 1),
        (i64::MIN, i64::MAX),
        (i64::MAX, i64::MIN),
        (-1, 0),
    ] {
        let sign = i64::from(x > y) - i64::from(x < y);
        for op in [
            CmpOp::Eq,
            CmpOp::Ne,
            CmpOp::Lt,
            CmpOp::Le,
            CmpOp::Gt,
            CmpOp::Ge,
        ] {
            assert_eq!(
                eval_cmp_i64(op, sign, 0),
                eval_cmp_i64(op, x, y),
                "{x} {op:?} {y}"
            );
            assert_eq!(
                eval_cmp_i64(op, 0, sign),
                eval_cmp_i64(op, y, x),
                "{y} {op:?} {x}"
            );
        }
    }
}
