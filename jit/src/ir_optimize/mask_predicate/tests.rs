    use super::*;
    use crate::ir::{MemKind, SafepointSnapshot};

    /// `for (k = 0; k < 16; k++) o = a[(i + k) & 7]` over the parameter `a`:
    /// returns `(graph, region, entry ctrl, array param, element read)`.
    fn masked_loop(mask: i64) -> (Graph, NodeId, NodeId, NodeId, NodeId) {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let a = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let i = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let region = g.add(Op::Merge, IrType::Control, vec![entry], Some(3));
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let k = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
        let next = g.add(Op::Add, IrType::Int, vec![k, one], None);
        g.nodes[k as usize].inputs.push(next);
        let sixteen = g.add(Op::Const(16), IrType::Int, vec![], None);
        let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![k, sixteen], None);
        let iff = g.add(Op::If, IrType::Control, vec![region, cond], Some(5));
        let stay = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        let sum = g.add(Op::Add, IrType::Int, vec![i, k], None);
        let m = g.add(Op::Const(mask), IrType::Int, vec![], None);
        let idx = g.add(Op::And, IrType::Int, vec![sum, m], None);
        let el = g.add(
            Op::ArrayLoad(MemKind::Ref),
            IrType::Ref,
            vec![stay, mem0, a, idx],
            Some(9),
        );
        let _ka = g.add(Op::KeepAlive, IrType::Void, vec![stay, el], Some(12));
        g.nodes[region as usize].inputs.push(stay);
        let ret = g.add(Op::Return, IrType::Void, vec![exit], None);
        g.exit = ret;
        // The header's own snapshot: locals `[a, i, k]`.
        g.push_safepoint(SafepointSnapshot::new(3, vec![a, i, k], vec![], vec![]));
        (g, region, entry, a, el)
    }

    #[test]
    fn a_constant_masked_access_over_an_invariant_array_gets_one_entry_guard() {
        let (mut g, region, entry, a, _) = masked_loop(7);
        // `loop_headers` classifies the back edge; ask it rather than guess.
        let headers = loop_headers(&g);
        let Some((_, e, backs)) = headers.iter().find(|(r, _, _)| *r == region).cloned() else {
            panic!("the hand-built loop must be recognised");
        };
        assert_eq!(e, entry);
        let plans = mask_predicate_plans(&mut g, region, entry, &backs);
        assert_eq!(plans.len(), 1);
        let plan = &plans[0];
        assert_eq!(plan.arrays, vec![a]);
        assert_eq!(plan.cmp, CmpOp::Lt);
        assert_eq!(g.nodes[plan.bound as usize].op, Op::Const(7));
        // The entry frame names `k` at its entry value.
        assert!(plan.frame.locals.contains(&a));
        assert!(!plan
            .frame
            .locals
            .iter()
            .any(|&v| g.nodes[v as usize].op == Op::Phi));
    }

    #[test]
    fn a_negative_mask_proves_nothing_and_plants_nothing() {
        let (mut g, region, entry, _, _) = masked_loop(-1);
        let backs = loop_headers(&g)
            .into_iter()
            .find(|(r, _, _)| *r == region)
            .map(|(_, _, b)| b)
            .expect("the loop is recognised");
        assert!(mask_predicate_plans(&mut g, region, entry, &backs).is_empty());
    }

    /// nio21 wave 3 (lane L5): the planted guard proves the masked access by
    /// graph shape alone, and only an access whose mask it covers.
    #[test]
    fn the_planted_guard_proves_the_masked_access_by_shape() {
        let (mut g, region, entry, _, el) = masked_loop(7);
        let backs = loop_headers(&g)
            .into_iter()
            .find(|(r, _, _)| *r == region)
            .map(|(_, _, b)| b)
            .expect("the loop is recognised");
        // No guard yet: nothing proves it.
        assert!(!mask_guard_proves_access(&g, &g.nodes[el as usize]));
        let plans = mask_predicate_plans(&mut g, region, entry, &backs);
        assert_eq!(plans.len(), 1);
        for plan in plans {
            assert_eq!(install_range_predicates(&mut g, plan), 1);
        }
        assert!(mask_guard_proves_access(&g, &g.nodes[el as usize]));
        // Widen the access's mask past the guard's constant: no longer proven.
        let idx = g.nodes[el as usize].inputs[3];
        let m = g.nodes[idx as usize].inputs[1];
        g.nodes[m as usize].op = Op::Const(15);
        assert!(!mask_guard_proves_access(&g, &g.nodes[el as usize]));
    }

    /// nio21 wave 5 (lane L5): `node_cannot_deopt` claims the proven masked
    /// load exactly when the emission's elision switch is on, and never
    /// before the guard exists or for an access the guard does not cover.
/// nio21 wave 7 (lane L5): the early plant is default ON (for the unroll
    /// candidates only), `=0` the kill switch; so are the two switches it
    /// serves.
/// nio21 wave 7 (lane L5, step 3): the lowering's cached proofs answer
    /// exactly what the per-access proof answers, before and after the plant
    /// and for a mask the guard does not cover.
    #[test]
    fn nio21w7_the_cached_proofs_agree_with_the_per_access_proof() {
        let (mut g, region, entry, _, el) = masked_loop(7);
        let agree = |g: &Graph, want: bool| {
            let n = &g.nodes[el as usize];
            assert_eq!(mask_guard_proves_access(g, n), want);
            assert_eq!(MaskGuardProofs::of(g).proves(g, n), want);
        };
        agree(&g, false);
        let backs = loop_headers(&g)
            .into_iter()
            .find(|(r, _, _)| *r == region)
            .map(|(_, _, b)| b)
            .expect("the loop is recognised");
        for plan in mask_predicate_plans(&mut g, region, entry, &backs) {
            assert_eq!(install_range_predicates(&mut g, plan), 1);
        }
        agree(&g, true);
        let idx = g.nodes[el as usize].inputs[3];
        let m = g.nodes[idx as usize].inputs[1];
        g.nodes[m as usize].op = Op::Const(15);
        agree(&g, false);
    }

    /// nio21 wave 7 (lane L5): a second plant (the late one, after an early
    /// guard the unroller did not consume) adds nothing for an array the
    /// existing guard already covers.
    #[test]
    fn nio21w7_the_late_plant_skips_an_array_an_early_guard_covers() {
        let (mut g, region, entry, _, _) = masked_loop(7);
        let backs = loop_headers(&g)
            .into_iter()
            .find(|(r, _, _)| *r == region)
            .map(|(_, _, b)| b)
            .expect("the loop is recognised");
        for plan in mask_predicate_plans(&mut g, region, entry, &backs) {
            assert_eq!(install_range_predicates(&mut g, plan), 1);
        }
        assert!(mask_predicate_plans(&mut g, region, entry, &backs).is_empty());
    }


    /// nio21 wave 6 (lane L5, L5W5-2): once the guard proves it, the fence
    /// row's element load is one the full unroll may drop, together with its
    /// `KeepAlive` and its private index (`(i + k) & 7`), and nothing the
    /// clone set or the carried `k` needs.
// ── JIT r20 wave 19, lane yLoopPred (LOOPOPT20-2 / -3 / -6) ──


    /// [`masked_loop`] with the opaque OSR merge the OSR door's build puts in
    /// front of every covered header (`IrBuilder::build_opaque_osr_merge`):
    /// `If(OsrEntryFlag)` on the entry, its arm and entry side joined by a
    /// `Merge` at the header bci, which is the header's forward input.
    /// Returns `(graph, region, that merge, element read)`.
    fn masked_loop_behind_arm(mask: i64) -> (Graph, NodeId, NodeId, NodeId) {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let a = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let i = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let flag = g.add(
            Op::OsrEntryFlag { header_bci: 3 },
            IrType::Int,
            vec![entry],
            None,
        );
        let gate = g.add(Op::If, IrType::Control, vec![entry, flag], None);
        let from_entry = g.add(Op::Proj(1), IrType::Control, vec![gate], None);
        let arm = g.add(Op::Proj(0), IrType::Control, vec![gate], None);
        let pre = g.add(Op::Merge, IrType::Control, vec![from_entry, arm], Some(3));
        let region = g.add(Op::Merge, IrType::Control, vec![pre], Some(3));
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let k = g.add(Op::Phi, IrType::Int, vec![region, zero], None);
        let next = g.add(Op::Add, IrType::Int, vec![k, one], None);
        g.nodes[k as usize].inputs.push(next);
        let sixteen = g.add(Op::Const(16), IrType::Int, vec![], None);
        let cond = g.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![k, sixteen], None);
        let iff = g.add(Op::If, IrType::Control, vec![region, cond], Some(5));
        let stay = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        let sum = g.add(Op::Add, IrType::Int, vec![i, k], None);
        let m = g.add(Op::Const(mask), IrType::Int, vec![], None);
        let idx = g.add(Op::And, IrType::Int, vec![sum, m], None);
        let el = g.add(
            Op::ArrayLoad(MemKind::Ref),
            IrType::Ref,
            vec![stay, mem0, a, idx],
            Some(9),
        );
        let _ka = g.add(Op::KeepAlive, IrType::Void, vec![stay, el], Some(12));
        g.nodes[region as usize].inputs.push(stay);
        let ret = g.add(Op::Return, IrType::Void, vec![exit], None);
        g.exit = ret;
        g.push_safepoint(SafepointSnapshot::new(3, vec![a, i, k], vec![], vec![]));
        (g, region, pre, el)
    }

    /// LOOPOPT20-3: on the optimizing OSR route the late plant takes a loop an
    /// opaque arm enters through its pre-header, and anchors the guard on that
    /// merge (which the arm falls into); a loop with no arm gets nothing there.
    #[test]
    fn yloop_loopopt20_3_the_osr_route_plants_only_behind_an_opaque_arm() {
        let despec = crate::deopt::DespecRegistry::new();
        let (mut g, region, pre, _) = masked_loop_behind_arm(7);
        assert!(super::super::entry_is_opaque_osr_arm(&g, pre));
        assert!(!super::super::entry_is_opaque_osr_arm(&g, region));
        g.optimize_osr_route = true;
        assert_eq!(plant_mask_predicates(&mut g, &despec, "T.m:()V", false), 1);
        assert!(
            mask_guards(&g).iter().any(|&(c, _, k)| c == pre && k == 7),
            "the guard is anchored on the merge the arm falls into"
        );
        let (mut h, _, _, _, _) = masked_loop(7);
        assert!(!super::super::entry_is_opaque_osr_arm(&h, h.entry));
        h.optimize_osr_route = true;
        assert_eq!(
            plant_mask_predicates(&mut h, &despec, "T.m:()V", false),
            0,
            "no arm: an OSR entry at the header would skip the guard"
        );
        let (mut h, _, entry, _, _) = masked_loop(7);
        assert_eq!(plant_mask_predicates(&mut h, &despec, "T.m:()V", false), 1);
        assert!(mask_guards(&h).iter().any(|&(c, _, _)| c == entry));
    }


    /// LOOPOPT20-6: the pure-body escape stops counting the pcs of the
    /// planted predicate (its guard and length name their own loop-entry
    /// frame) and of a pure node; a control node and an unnamed guard still
    /// count.
    #[test]
    fn yloop_loopopt20_6_a_named_predicate_and_a_pure_node_read_no_flat_frame() {
        let (mut g, region, entry, _, _) = masked_loop(7);
        let backs = loop_headers(&g)
            .into_iter()
            .find(|(r, _, _)| *r == region)
            .map(|(_, _, b)| b)
            .expect("the loop is recognised");
        for plan in mask_predicate_plans(&mut g, region, entry, &backs) {
            assert_eq!(install_range_predicates(&mut g, plan), 1);
        }
        let guard = g
            .nodes
            .iter()
            .position(|n| matches!(n.op, Op::Guard { .. }))
            .expect("the planted guard") as NodeId;
        let cond = g.nodes[guard as usize].inputs[1];
        let len = g.nodes[cond as usize].inputs[1];
        assert_eq!(g.nodes[len as usize].op, Op::ArrayLength);
        let reads_none =
            |g: &Graph, id: NodeId| super::super::pc_reads_no_flat_frame(g, &g.nodes[id as usize]);
        assert!(
            reads_none(&g, guard),
            "the guard boxes its own loop-entry frame"
        );
        assert!(
            reads_none(&g, len),
            "the length's null check resolves its own frame"
        );
        assert!(reads_none(&g, cond), "a compare cannot deopt");
        assert!(
            !reads_none(&g, region),
            "a control node: an OSR entry stub seeds from the flat frame there"
        );
        let bare = g.add(
            Op::Guard { bci: 3 },
            IrType::Void,
            vec![entry, cond],
            Some(3),
        );
        assert!(
            !reads_none(&g, bare),
            "an unnamed guard boxes the flat frame"
        );
    }

    /// Build `code` (a static `(int[] t, int i) -> int`) and run the whole
    /// optimizing pipeline with a de-spec registry, under `overrides`.
    fn yloop_build_and_optimize(
        code: &[u8],
        code_len: usize,
        num_locals: usize,
        overrides: &[(&str, Option<&str>)],
    ) -> Graph {
        cratonvm_types::flags::with_thread_overrides(overrides, || {
            let mut builder = crate::ir::IrBuilder::new(2, num_locals);
            builder.set_param_types(&[IrType::Ref, IrType::Int]);
            let mut g = builder.build(code, code_len).expect("IR build");
            let despec = crate::deopt::DespecRegistry::new();
            super::super::optimize_with(
                &mut g,
                super::super::OptimizeContext {
                    despec: Some(&despec),
                    method_key: "R20YLoopPred.f:([II)I",
                    osr_route: false,
                },
            );
            g
        })
    }

    /// The live `ArrayLoad`s of `g`.
    fn yloop_array_loads(g: &Graph) -> Vec<NodeId> {
        g.nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| matches!(n.op, Op::ArrayLoad(_)))
            .map(|(id, _)| id as NodeId)
            .collect()    }