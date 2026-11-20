use super::{array_element_promote_enabled, fresh_promote_static_feed_enabled};
use super::promote_loop_array_elements;
use crate::ir::{Graph, IrBuilder, IrType, MemKind, NodeId, Op};

/// `static void f(int[] a, int n) { for (int i = 0; i < n; i++) a[0] += i; }`
/// -- header at bci 2, `iaload` at 10, `iastore` at 13.
const ACC_ZERO: [u8; 21] = [
    0x03, 0x3d, // 0: i = 0
    0x1c, 0x1b, 0xa2, 0x00, 0x10, // 2: header: if (i >= n) goto 20
    0x2a, 0x03, 0x5c, 0x2e, 0x1c, 0x60, 0x4f, // 7: a[0] = a[0] + i
    0x84, 0x02, 0x01, 0xa7, 0xff, 0xf1, // 14: i++; 17: goto 2
    0xb1, // 20: return
];

/// `for (..) a[0] += a[i];` -- a second `int` element of the same array
/// (it may be the cell itself when `i == 0`).
const ACC_ALIAS: [u8; 23] = [
    0x03, 0x3d, // 0: i = 0
    0x1c, 0x1b, 0xa2, 0x00, 0x12, // 2: header: if (i >= n) goto 22
    0x2a, 0x03, 0x5c, 0x2e, 0x2a, 0x1c, 0x2e, 0x60, 0x4f, // 7: a[0] = a[0] + a[i]
    0x84, 0x02, 0x01, 0xa7, 0xff, 0xef, // 16: i++; 19: goto 2
    0xb1, // 22: return
];

fn build(code: &[u8]) -> Graph {
    let params = [IrType::Ref, IrType::Int];
    let mut b = IrBuilder::new(params.len(), 3);
    b.set_param_types(&params);
    let mut g = b.build(code, code.len()).expect("builds");
    super::super::scalar_cleanup(&mut g);
    g
}

fn live(g: &Graph, f: impl Fn(&Op) -> bool) -> Vec<NodeId> {
    (0..g.nodes.len())
        .filter(|&i| f(&g.nodes[i].op))
        // Cast: arena index to node id.
        .map(|i| i as NodeId)
        .collect()
}

/// `a[0] += i`: the in-loop load and store go; the pre-header holds the
/// length, two index guards (with the pre-header frame) and the read; one
/// conditional store at the header's bci runs on the exit edge.
#[test]
fn r21_loops_an_invariant_element_accumulator_is_promoted() {
    let mut g = build(&ACC_ZERO);
    assert_eq!(promote_loop_array_elements(&mut g, None), 1);
    assert_eq!(g.promoted_cell_header_bcis, vec![2]);
    assert!(
        g.promoted_cell_writebacks.is_empty(),
        "the poll pad has no element shape: no record, so its exit is refused"
    );
    let stores = live(&g, |op| matches!(op, Op::ArrayStore(MemKind::Int)));
    let [st] = stores.as_slice() else {
        panic!("one store left, got {}", stores.len());
    };
    assert_eq!(g.nodes[*st as usize].bytecode_pc, Some(2));
    let taken = g.nodes[*st as usize].inputs[0];
    assert_eq!(g.nodes[taken as usize].op, Op::Proj(0), "conditional");
    let loads = live(&g, |op| matches!(op, Op::ArrayLoad(MemKind::Int)));
    assert!(
        loads.iter().all(|&l| g.nodes[l as usize].bytecode_pc == Some(2)),
        "only the pre-header read is left"
    );
    let guards: Vec<NodeId> = live(&g, |op| matches!(op, Op::Guard { bci: 2 }));
    assert_eq!(guards.len(), 2, "0 <= k and k < a.length");
    assert!(guards.iter().all(|&x| g.nodes[x as usize].frame_snapshot.is_some()));
    assert!(crate::ir_verify::verify_graph_at_phase(&g, "post-optimize").is_ok());
}

/// A second element of the same kind (it may alias the cell) or a
/// header the registry withdrew keeps every access.
#[test]
fn r21_loops_an_aliasing_access_or_a_withdrawn_header_keeps_the_loop() {
    let mut g = build(&ACC_ALIAS);
    assert_eq!(promote_loop_array_elements(&mut g, None), 0);
    assert!(g.promoted_cell_header_bcis.is_empty());
    assert_eq!(live(&g, |op| matches!(op, Op::ArrayStore(_))).len(), 1);
    assert!(live(&g, |op| matches!(op, Op::Guard { .. })).is_empty());

    let mut h = build(&ACC_ZERO);
    let reg = crate::deopt::DespecRegistry::new();
    reg.insert("T.f:([II)V", 2, crate::deopt::SPECULATION_ID_ANY);
    assert_eq!(promote_loop_array_elements(&mut h, Some((&reg, "T.f:([II)V"))), 0);
    assert!(h.promoted_cell_header_bcis.is_empty());
    assert!(live(&h, |op| matches!(op, Op::ArrayStore(_)))
        .iter()
        .any(|&s| h.nodes[s as usize].bytecode_pc == Some(13)));
}


