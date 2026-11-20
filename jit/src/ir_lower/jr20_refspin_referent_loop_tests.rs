use super::*;
use crate::ir::{IrBuilder, IrInlineSite, IrInlineTables};
use crate::ir_schedule;

/// `static int spin(R r, int limit) { int n = 0; while (get(r) != null &&
/// n < limit) n++; return n; }`, with `static Object get(R r) { return
/// r.referent; }` spliced at caller pc 3 and relocated to combined pc 24:
///
/// ```text
///  0 iconst_0          1 istore_2
///  2 aload_0           3 invokestatic #1  (spliced)
///  6 ifnull 20         9 iload_2   10 iload_1   11 if_icmpge 20
/// 14 iinc 2, 1        17 goto 2
/// 20 iload_2          21 ireturn
/// 24 aload_0          25 getfield #2 (referent)   28 areturn
/// ```
const CODE: [u8; 31] = [
    0x03, 0x3d, 0x2a, 0xb8, 0x00, 0x01, 0xc6, 0x00, 0x0e, 0x1c, 0x1b, 0xa2, 0x00, 0x09, 0x84,
    0x02, 0x01, 0xa7, 0xff, 0xf1, 0x1c, 0xac, 0x00, 0x00, // caller [0, 22) + pad
    0x2a, 0xb4, 0x00, 0x02, 0xb0, // relocated callee [24, 29)
    0x00, 0x00,
];
const REFERENT_PC: usize = 25;

/// Build and optimise the loop; `referent` lists pc 25 the way
/// `append_ir_inline_site` rebases `InlineSite::referent_load_pcs`.
fn build(referent: bool) -> Graph {
    let site = IrInlineSite {
        base: 24,
        code_len: 5,
        num_args: 1,
        max_locals: 1,
        arg_local_slots: vec![0],
        returns_value: true,
        receiver_is_arg0: false,
        method_key: "R.get:(LR;)Ljava/lang/Object;".to_string(),
        class_id: 7,
    };
    let mut b = IrBuilder::new(2, 3);
    b.set_param_types(&[IrType::Ref, IrType::Int]);
    let mut tables = IrInlineTables::default();
    tables.sites.insert(3, site);
    tables.field_info.insert(REFERENT_PC, (0usize, b'L'));
    if referent {
        tables.referent_load_pcs.insert(REFERENT_PC);
    }
    b.apply_inline_tables(tables);
    let mut g = b.build(&CODE, 22).expect("the spliced loop builds");
    crate::ir_optimize::optimize(&mut g);
    g
}

/// The scheduled reads at the referent pc (unrolled copies included).
fn referent_loads(g: &Graph, s: &Schedule) -> Vec<usize> {
    g.nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| matches!(n.op, Op::Load(_)) && n.bytecode_pc == Some(REFERENT_PC))
        .map(|(id, _)| id)
        .filter(|&id| s.node_to_block.get(id).is_some_and(|&b| b < s.blocks.len()))
        .collect()
}

/// `(latch, header)` of every reducible back edge.
fn back_edges(s: &Schedule) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (b, blk) in s.blocks.iter().enumerate() {
        for &h in &blk.successors {
            if h <= b && s.dom.dominates(h, b) {
                out.push((b, h));
            }
        }
    }
    out
}

/// The natural loop of `latch -> header`, as block indices.
fn natural_loop(s: &Schedule, latch: usize, header: usize) -> Vec<usize> {
    let mut body = vec![header];
    let mut work = vec![latch];
    while let Some(b) = work.pop() {
        if body.contains(&b) {
            continue;
        }
        body.push(b);
        work.extend(s.blocks[b].predecessors.iter().copied());
    }
    body
}

/// The referent read is re-read every iteration: it survives the
/// optimizer (not forwarded, CSE'd or hoisted away), stays volatile-ordered
/// and sits inside the loop's body; and at the loop's back-edge poll its
/// home is one `emit_back_edge_dead_home_clears` zeroes, so the poll's map
/// stops rooting the last iteration's referent (the defect: the collector
/// never cleared it while the loop spun).
#[test]
fn jr20_refspin_a_spliced_referent_read_stays_in_the_loop_and_dies_at_its_back_edge() {
    let g = build(true);
    assert!(g.referent_load_pcs.contains(&REFERENT_PC));
    let schedule = ir_schedule::schedule(&g);
    let loads = referent_loads(&g, &schedule);
    assert!(
        !loads.is_empty(),
        "the referent read survives optimisation (never forwarded or removed)"
    );
    for &l in &loads {
        assert!(
            g.nodes[l].is_volatile_access(),
            "n{l}: a referent read is ordered like a volatile one"
        );
    }
    let edges = back_edges(&schedule);
    assert!(!edges.is_empty(), "the loop survives optimisation");
    for &l in &loads {
        let home = schedule.node_to_block[l];
        assert!(
            edges
                .iter()
                .any(|&(latch, header)| natural_loop(&schedule, latch, header).contains(&home)),
            "n{l} (block {home}) is not inside the loop: it was hoisted out"
        );
    }
    let plan = plan_slots(&g, &schedule, None);
    let dead: Vec<usize> = edges
        .iter()
        .flat_map(|&(latch, _)| referent_loads_dead_at_back_edge(&g, &schedule, &plan, latch))
        .map(|(id, _)| id)
        .collect();
    for &l in &loads {
        assert!(
            dead.contains(&l),
            "n{l}: the referent read's home is dead at its loop's back-edge poll \
             and must be zeroed there (dead: {dead:?})"
        );
    }
}

/// Control: the same loop with no referent pc builds an ordinary read,
/// and nothing is zeroed at its back edge.
#[test]
fn jr20_refspin_an_ordinary_field_read_is_not_cleared_at_the_back_edge() {
    let g = build(false);
    assert!(g.referent_load_pcs.is_empty());
    let schedule = ir_schedule::schedule(&g);
    let plan = plan_slots(&g, &schedule, None);
    for (latch, _) in back_edges(&schedule) {
        assert!(referent_loads_dead_at_back_edge(&g, &schedule, &plan, latch).is_empty());
    }
}
