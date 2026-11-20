use super::*;
use crate::ir::IrBuilder;
use crate::ir_schedule;

/// `static int walk(Node head) { int n = 0; Node p = head; while (p !=
/// null) { if (p.val != null) n++; p = p.next; } return n; }` -- a
/// call-free loop with a dead `Ref` temporary (`p.val`, read only by the
/// `ifnull`) and a `Ref` carried round the loop (`p`, a header phi whose
/// latch edge reads `p.next`):
///
/// ```text
///  0 iconst_0          1 istore_1          2 aload_0           3 astore_2
///  4 aload_2           5 ifnull 26
///  8 aload_2           9 getfield #1 (val) 12 ifnull 18        15 iinc 1, 1
/// 18 aload_2          19 getfield #2 (next) 22 astore_2        23 goto 4
/// 26 iload_1          27 ireturn
/// ```
const WALK: [u8; 28] = [
    0x03, 0x3c, 0x2a, 0x4d, 0x2c, 0xc6, 0x00, 0x15, 0x2c, 0xb4, 0x00, 0x01, 0xc6, 0x00, 0x06,
    0x84, 0x01, 0x01, 0x2c, 0xb4, 0x00, 0x02, 0x4d, 0xa7, 0xff, 0xed, 0x1b, 0xac,
];
const WALK_VAL_PC: usize = 9;
const WALK_NEXT_PC: usize = 19;

/// `static Object last(Node head) { Node p = head; Object v; do { v =
/// p.val; p = p.next; } while (p != null); return v; }` -- the latch's
/// branch also LEAVES the loop, and `v` is read after it:
///
/// ```text
///  0 aload_0           1 astore_1
///  2 aload_1           3 getfield #1 (val)  6 astore_2
///  7 aload_1           8 getfield #2 (next) 11 astore_1
/// 12 aload_1          13 ifnonnull 2
/// 16 aload_2          17 areturn
/// ```
const LAST: [u8; 18] = [
    0x2a, 0x4c, 0x2b, 0xb4, 0x00, 0x01, 0x4d, 0x2b, 0xb4, 0x00, 0x02, 0x4c, 0x2b, 0xc7, 0xff,
    0xf5, 0x2c, 0xb0,
];
const LAST_VAL_PC: usize = 3;

/// Build and optimise a one-parameter (`Node`) method whose two
/// `getfield`s at `val_pc` / `next_pc` read reference fields 0 and 1.
fn build(code: &[u8], val_pc: usize, next_pc: usize) -> Graph {
    let mut b = IrBuilder::new(1, 3);
    b.set_param_types(&[IrType::Ref]);
    let mut fi = std::collections::HashMap::new();
    fi.insert(val_pc, (0usize, b'L'));
    fi.insert(next_pc, (1usize, b'L'));
    b.set_field_info(fi);
    let mut g = b.build(code, code.len()).expect("the list walk builds");
    crate::ir_optimize::optimize(&mut g);
    g
}

/// The scheduled `Ref` reads at `pc` (unrolled copies included).
fn ref_loads_at(g: &Graph, s: &Schedule, pc: usize) -> Vec<usize> {
    g.nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| {
            matches!(n.op, Op::Load(_)) && n.ty == IrType::Ref && n.bytecode_pc == Some(pc)
        })
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

/// Every value reported dead at any back edge, general or referent mode.
fn dead_anywhere(g: &Graph, s: &Schedule, plan: &SlotPlan, general: bool) -> Vec<usize> {
    back_edges(s)
        .iter()
        .flat_map(|&(latch, _)| back_edge_dead_ref_homes(g, s, plan, latch, general))
        .map(|(id, _)| id)
        .collect()
}

/// The defect: `p.val` is dead at the back-edge poll (nothing after the
/// `ifnull` reads it), yet the poll's map published its home, so the
/// previous iteration's value stayed a strong root at every collection
/// stopping there. It is reported now (general mode) -- and NOT under the
/// `=0` arm, which keeps only the referent reads (there are none).
#[test]
fn jr20_pollroot_a_dead_ref_temporary_of_a_call_free_loop_is_cleared_at_its_back_edge() {
    let g = build(&WALK, WALK_VAL_PC, WALK_NEXT_PC);
    assert!(
        !g.nodes.iter().any(|n| matches!(n.op, Op::Call { .. })),
        "the loop body makes no call: the back-edge poll is its only GC point"
    );
    let s = ir_schedule::schedule(&g);
    let vals = ref_loads_at(&g, &s, WALK_VAL_PC);
    assert!(!vals.is_empty(), "the p.val read survives optimisation");
    assert!(!back_edges(&s).is_empty(), "the loop survives optimisation");
    let plan = plan_slots(&g, &s, None);
    let dead = dead_anywhere(&g, &s, &plan, true);
    for &v in &vals {
        assert!(
            dead.contains(&v),
            "n{v} (p.val) is dead at the back-edge poll and must leave its home there \
             (dead: {dead:?})"
        );
    }
    assert!(
        dead_anywhere(&g, &s, &plan, false).is_empty(),
        "CRATONVM_JIT_IR_BACK_EDGE_DEAD_HOME_CLEAR=0: no referent read, nothing cleared"
    );
    for (latch, _) in back_edges(&s) {
        assert!(referent_loads_dead_at_back_edge(&g, &s, &plan, latch).is_empty());
    }
}

/// `p` is carried round the loop in a header phi whose latch edge reads
/// `p.next`. The edge value is live over the poll and never reported; the
/// phi itself is (wave 21 follow-up): its last reader, the `p.next` load,
/// ran before the poll, and the edge copy that rewrites its home runs
/// after it, so the home holds the node the loop has just left. Not under
/// the referent-only mode.
#[test]
fn jr20_pollroot_a_header_phi_dead_at_the_poll_is_cleared_but_not_its_edge_value() {
    let g = build(&WALK, WALK_VAL_PC, WALK_NEXT_PC);
    let s = ir_schedule::schedule(&g);
    let plan = plan_slots(&g, &s, None);
    let edges = back_edges(&s);
    assert!(!edges.is_empty(), "the loop survives optimisation");
    let mut header_ref_phis = 0usize;
    let mut reported = 0usize;
    for &(latch, header) in &edges {
        let dead: Vec<usize> = back_edge_dead_ref_homes(&g, &s, &plan, latch, true)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        for (id, n) in g.nodes.iter().enumerate() {
            if !matches!(n.op, Op::Phi) || n.ty != IrType::Ref {
                continue;
            }
            if n.inputs.as_slice().first().copied() != Some(s.blocks[header].ctrl) {
                continue;
            }
            header_ref_phis += 1;
            if dead.contains(&id) {
                reported += 1;
                let read_by_a_phi = g.nodes.iter().any(|q| {
                    matches!(q.op, Op::Phi)
                        && q.inputs
                            .as_slice()
                            .iter()
                            .skip(1)
                            .any(|&v| v as usize == id)
                });
                assert!(!read_by_a_phi, "header phi n{id} is an edge input of a phi");
            }
            for &v in n.inputs.as_slice().iter().skip(1) {
                assert!(
                    !dead.contains(&(v as usize)),
                    "n{v} feeds header phi n{id}: live over the poll, never cleared"
                );
            }
        }
    }
    assert!(header_ref_phis > 0, "the list cursor p is a header phi");
    assert!(reported > 0, "the cursor's header phi is dead at the poll");
    assert!(dead_anywhere(&g, &s, &plan, false).is_empty());
}

/// `static int swap(Object x, Object y, int n) { while (n > 0) { Object t =
/// x; x = y; y = t; n--; } return x == null ? 0 : 1; }`: the header phis
/// read each other on the back edge (a parallel-copy cycle).
///
/// ```text
///  0 iload_2           1 ifle 16
///  4 aload_0  5 astore_3  6 aload_1  7 astore_0  8 aload_3  9 astore_1
/// 10 iinc 2, -1       13 goto 0
/// 16 aload_0          17 ifnonnull 22     20 iconst_0  21 ireturn
/// 22 iconst_1         23 ireturn
/// ```
const SWAP: [u8; 24] = [
    0x1c, 0x9e, 0x00, 0x0f, 0x2a, 0x4e, 0x2b, 0x4b, 0x2d, 0x4c, 0x84, 0x02, 0xff, 0xa7, 0xff,
    0xf3, 0x2a, 0xc7, 0x00, 0x05, 0x03, 0xac, 0x04, 0xac,
];

/// A header phi another phi reads on the back edge is never cleared: that
/// copy runs after the poll and reads the phi's home.
#[test]
fn jr20_pollroot_header_phis_that_swap_are_not_cleared() {
    let mut b = IrBuilder::new(3, 4);
    b.set_param_types(&[IrType::Ref, IrType::Ref, IrType::Int]);
    let mut g = b.build(&SWAP, SWAP.len()).expect("the swap loop builds");
    crate::ir_optimize::optimize(&mut g);
    let s = ir_schedule::schedule(&g);
    let plan = plan_slots(&g, &s, None);
    let dead = dead_anywhere(&g, &s, &plan, true);
    for &id in &dead {
        let read_by_a_header_phi = back_edges(&s).iter().any(|&(_, header)| {
            g.nodes.iter().any(|q| {
                matches!(q.op, Op::Phi)
                    && q.inputs.as_slice().first().copied() == Some(s.blocks[header].ctrl)
                    && q.inputs
                        .as_slice()
                        .iter()
                        .skip(1)
                        .any(|&v| v as usize == id)
            })
        });
        assert!(
            !read_by_a_header_phi,
            "n{id} is read by a header phi's edge copy after the poll (dead: {dead:?})"
        );
    }
}

/// `static int count(Node head) { int n = 0; Node p = head; Object v; do
/// { v = p.val; if (v == null) n++; p = p.next; } while (p != null);
/// return n; }` -- the latch's branch leaves the loop, `v` is not read
/// after it:
///
/// ```text
///  0 iconst_0  1 istore_1  2 aload_0  3 astore_2
///  4 aload_2   5 getfield #1 (val)  8 astore_3
///  9 aload_3  10 ifnonnull 16       13 iinc 1, 1
/// 16 aload_2  17 getfield #2 (next) 20 astore_2  21 aload_2  22 ifnonnull 4
/// 25 iload_1  26 ireturn
/// ```
const COUNT: [u8; 27] = [
    0x03, 0x3c, 0x2a, 0x4d, 0x2c, 0xb4, 0x00, 0x01, 0x4e, 0x2d, 0xc7, 0x00, 0x06, 0x84, 0x01,
    0x01, 0x2c, 0xb4, 0x00, 0x02, 0x4d, 0x2c, 0xc7, 0xff, 0xee, 0x1b, 0xac,
];
const COUNT_VAL_PC: usize = 5;
const COUNT_NEXT_PC: usize = 17;

/// The exit rule's precise snapshot half (wave 21 follow-up) is asked only
/// at a latch that can leave the loop. The scheduler gives the do-while's
/// taken back edge a block of its own (the `If`'s projection), so the
/// latch's one successor is the header: the exit leaves from the branch
/// block, before the poll, and the first-arrival rule alone clears `v`
/// (dead on every path from the poll). The keep union is never consulted
/// there. (Host w21r, 2026-10-13: the earlier form of this test asserted
/// an exiting latch and failed on that layout.)
#[test]
fn jr20_pollroot_a_do_while_back_edge_latch_clears_without_asking_the_exit_rule() {
    let mut b = IrBuilder::new(1, 4);
    b.set_param_types(&[IrType::Ref]);
    let mut fi = std::collections::HashMap::new();
    fi.insert(COUNT_VAL_PC, (0usize, b'L'));
    fi.insert(COUNT_NEXT_PC, (1usize, b'L'));
    b.set_field_info(fi);
    let mut g = b.build(&COUNT, COUNT.len()).expect("the do-while builds");
    crate::ir_optimize::optimize(&mut g);
    let s = ir_schedule::schedule(&g);
    let plan = plan_slots(&g, &s, None);
    let vals = ref_loads_at(&g, &s, COUNT_VAL_PC);
    assert!(!vals.is_empty(), "the p.val read survives optimisation");
    let edges = back_edges(&s);
    assert!(!edges.is_empty(), "the loop survives optimisation");
    for &(latch, header) in &edges {
        assert_eq!(
            s.blocks[latch].successors,
            vec![header],
            "the do-while's back edge is a block of its own"
        );
    }
    let words = plan.slots.div_ceil(64).max(1);
    let mut asked = 0usize;
    let mut dead = Vec::new();
    for &(latch, _) in &edges {
        let mut reach = |_pc: usize| {
            asked += 1;
            Some(vec![u64::MAX; words])
        };
        dead.extend(
            back_edge_dead_ref_homes_with(&g, &s, &plan, latch, true, Some(&mut reach))
                .into_iter()
                .map(|(id, _)| id),
        );
    }
    assert_eq!(
        asked, 0,
        "a latch without an exit never asks the keep union"
    );
    for &v in &vals {
        assert!(
            dead.contains(&v),
            "n{v} (v = p.val) is dead on every path from the poll"
        );
    }
    // Referent-only mode never clears it.
    assert!(dead_anywhere(&g, &s, &plan, false).is_empty());
}

/// Control: a dead value whose colour another value live over the poll
/// also lives in is not cleared -- the word holds the live one. The
/// sharing is forced on the plan (the colouring itself only shares
/// non-overlapping ranges, so the live tenant is whichever value's range
/// covers the poll).
#[test]
fn jr20_pollroot_a_colour_shared_with_a_value_live_at_the_poll_is_not_cleared() {
    let g = build(&WALK, WALK_VAL_PC, WALK_NEXT_PC);
    let s = ir_schedule::schedule(&g);
    let mut plan = plan_slots(&g, &s, None);
    let (latch, v) = back_edges(&s)
        .into_iter()
        .find_map(|(latch, _)| {
            back_edge_dead_ref_homes(&g, &s, &plan, latch, true)
                .first()
                .map(|&(v, _)| (latch, v))
        })
        .expect("the walk has a dead temporary at some back edge");
    let poll = back_edge_poll_pos(&s, &plan, latch).expect("the poll's position is known");
    let ranges: Vec<Option<LiveRange>> = plan.colouring_ranges().to_vec();
    let live = (0..g.nodes.len())
        .find(|&m| {
            m != v
                && plan.node_color.get(m).copied().flatten().is_some()
                && ranges
                    .get(m)
                    .copied()
                    .flatten()
                    .is_some_and(|r| r.lo <= poll && poll <= r.hi)
        })
        .expect("some value is live over the poll");
    plan.node_color[live] = plan.node_color[v];
    let dead = back_edge_dead_ref_homes(&g, &s, &plan, latch, true);
    assert!(
        !dead.iter().any(|&(id, _)| id == v),
        "n{v} shares its word with n{live}, live over the poll: not cleared"
    );
}

/// Review fix (unconditional, both modes): `v = p.val` is read after a
/// do-while. A latch that could leave the loop would take the path out
/// without re-running the body, so it must keep `v`; a latch whose only
/// successor is its header re-runs `v` before any read and may clear it.
/// The scheduler gives the taken back edge its own block (host w21r,
/// 2026-10-13: `v` IS reported dead there, which the earlier form of this
/// test, asserting "never cleared", got wrong).
#[test]
fn jr20_pollroot_a_value_read_after_the_loop_is_cleared_only_where_the_path_re_enters_the_loop()
{
    let g = build(&LAST, LAST_VAL_PC, 8);
    let s = ir_schedule::schedule(&g);
    let vals = ref_loads_at(&g, &s, LAST_VAL_PC);
    assert!(!vals.is_empty(), "the p.val read survives optimisation");
    let edges = back_edges(&s);
    assert!(!edges.is_empty(), "the loop survives optimisation");
    let plan = plan_slots(&g, &s, None);
    for &(latch, _) in &edges {
        let exits = s.blocks[latch].successors.iter().any(|&x| x > latch);
        let dead = back_edge_dead_ref_homes(&g, &s, &plan, latch, true);
        for &v in &vals {
            if dead.iter().any(|&(id, _)| id == v) {
                assert!(
                    !exits,
                    "n{v} (v = p.val) is returned after the loop: cleared at an exiting latch"
                );
            }
        }
    }
}

/// `static int run(Holder h, int m) { int n = 0; while (m > 0) { Object
/// big = h.node; if (big != null) n++; while (n < m) n++; m--; } return
/// n; }` -- `big` is dead throughout the inner call-free loop:
///
/// ```text
///  0 iconst_0   1 istore_2
///  2 iload_1    3 ifle 35
///  6 aload_0    7 getfield #1 (node)  10 astore_3
/// 11 aload_3   12 ifnull 18           15 iinc 2, 1
/// 18 iload_2   19 iload_1             20 if_icmpge 29   23 iinc 2, 1   26 goto 18
/// 29 iinc 1, -1                       32 goto 2
/// 35 iload_2   36 ireturn
/// ```
const OUTER_DEAD: [u8; 37] = [
    0x03, 0x3d, 0x1b, 0x9e, 0x00, 0x20, 0x2a, 0xb4, 0x00, 0x01, 0x4e, 0x2d, 0xc6, 0x00, 0x06,
    0x84, 0x02, 0x01, 0x1c, 0x1b, 0xa2, 0x00, 0x09, 0x84, 0x02, 0x01, 0xa7, 0xff, 0xf8, 0x84,
    0x01, 0xff, 0xa7, 0xff, 0xe2, 0x1c, 0xac,
];

/// The same with `big` read AFTER the inner loop (live throughout it):
///
/// ```text
///  0 iconst_0   1 istore_2
///  2 iload_1    3 ifle 35
///  6 aload_0    7 getfield #1 (node)  10 astore_3
/// 11 iload_2   12 iload_1             13 if_icmpge 22   16 iinc 2, 1   19 goto 11
/// 22 aload_3   23 ifnull 29           26 iinc 2, 1
/// 29 iinc 1, -1                       32 goto 2
/// 35 iload_2   36 ireturn
/// ```
const OUTER_LIVE: [u8; 37] = [
    0x03, 0x3d, 0x1b, 0x9e, 0x00, 0x20, 0x2a, 0xb4, 0x00, 0x01, 0x4e, 0x1c, 0x1b, 0xa2, 0x00,
    0x09, 0x84, 0x02, 0x01, 0xa7, 0xff, 0xf8, 0x2d, 0xc6, 0x00, 0x06, 0x84, 0x02, 0x01, 0x84,
    0x01, 0xff, 0xa7, 0xff, 0xe2, 0x1c, 0xac,
];
const OUTER_BIG_PC: usize = 7;

/// Build, optimise and schedule `code`, plan its slots and its
/// dead-reference facts (precise keep set); then, at the poll of every
/// back edge whose loop does NOT contain the `big` read, the poll rule's
/// verdict on `big`'s colour: `(dead, outer, read by a value phi)`.
fn outer_verdicts(code: &[u8]) -> Vec<(bool, bool, bool)> {
    let mut b = IrBuilder::new(2, 4);
    b.set_param_types(&[IrType::Ref, IrType::Int]);
    let mut fi = std::collections::HashMap::new();
    fi.insert(OUTER_BIG_PC, (0usize, b'L'));
    b.set_field_info(fi);
    let mut g = b.build(code, code.len()).expect("the nested loop builds");
    crate::ir_optimize::optimize(&mut g);
    let s = ir_schedule::schedule(&g);
    let plan = plan_slots(&g, &s, None);
    let bigs = ref_loads_at(&g, &s, OUTER_BIG_PC);
    assert!(!bigs.is_empty(), "the big read survives optimisation");
    let mut facts = plan_dead_ref_slot_clear(
        &g,
        &s,
        &plan,
        DeadRefClearBytecode {
            code,
            method_descriptor: "(Ljava/lang/Object;I)V",
            method_synchronized: false,
            num_params: 2,
            num_locals: 4,
            handlers: &[],
            spliced_bodies: &[],
        },
        &[],
        true,
        None,
        false,
    )
    .expect("the dead-reference facts build");
    let mut out = Vec::new();
    for (latch, header) in back_edges(&s) {
        for &big in &bigs {
            let big_block = s.node_to_block[big];
            if s.dom.dominates(header, big_block) {
                continue; // the outer loop: the dominance rule's case
            }
            let pos = back_edge_poll_pos(&s, &plan, latch).expect("poll position");
            let bci = back_edge_poll_bci(&g, &s, latch).expect("poll bci");
            let colour = plan.node_color[big].expect("big has a home");
            let dead = facts
                .dead_colours_at_back_edge_poll(pos, bci, s.blocks[latch].ctrl)
                .contains(&colour);
            let outer = outer_ref_colours(&g, &s, &plan, &[header], &|_| false)
                .get(colour as usize)
                .copied()
                .unwrap_or(false);
            let read_by_phi = g.nodes.iter().any(|q| {
                matches!(q.op, Op::Phi)
                    && q.inputs
                        .as_slice()
                        .iter()
                        .skip(1)
                        .any(|&v| v as usize == big)
            });
            out.push((dead, outer, read_by_phi));
        }
    }
    out
}

/// The defect of `jr20-inner-loop-poll-roots-dead-outer-loop-values-FIXED-20261013.md`:
/// `big`, defined before the inner loop and read by nothing after it, is
/// dead at the inner loop's poll, and its colour is an outer one -- so the
/// poll zeroes it. (A header phi carrying it round the outer loop would be
/// a reader; then it must be kept.)
#[test]
fn jr20_pollroot_an_outer_value_dead_across_an_inner_loop_is_cleared_at_its_poll() {
    let verdicts = outer_verdicts(&OUTER_DEAD);
    assert!(
        !verdicts.is_empty(),
        "an inner loop that does not contain big"
    );
    for (dead, outer, read_by_phi) in verdicts {
        assert!(
            outer,
            "big's colour holds only values defined outside the inner loop"
        );
        if read_by_phi {
            assert!(!dead, "a phi carries big on: kept");
        } else {
            assert!(
                dead,
                "big is dead throughout the inner loop: cleared at its poll"
            );
        }
    }
}

/// Control: `big` is read after the inner loop, so it is live at the
/// inner poll and never cleared.
#[test]
fn jr20_pollroot_an_outer_value_read_after_the_inner_loop_is_kept() {
    let verdicts = outer_verdicts(&OUTER_LIVE);
    assert!(
        !verdicts.is_empty(),
        "an inner loop that does not contain big"
    );
    for (dead, _, _) in verdicts {
        assert!(!dead, "big is read after the inner loop: live at its poll");
    }
}
