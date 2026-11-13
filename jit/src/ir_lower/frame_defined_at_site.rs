// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 eager-guards soak close
//! (`docs/feature-designs/jep358-helpful-npe.md`):
//! a deopting field guard may publish a frame only through values that are
//! already DEFINED where the guard sits.
//!
//! LICM moves `this.hashes` of `while (hashes[j] > pivot) j--` to the loop's
//! pre-header. The read keeps its in-loop snapshot, which names the header
//! phi `j`; that phi's register is written by the phi copies at the END of the
//! pre-header, after the hoisted read and its compactness guard. A guard that
//! trips there (`CRATONVM_DEOPT_EAGER_CHAINS` forces one) rebuilds the
//! interpreter frame from a register holding another value's bits: a
//! `ZipContent$Loader.sort` index of -2080847088 on the Spring Boot smoke jar.
//! `register_frame_value` answers "has a register" for every phi whatever the
//! position, so the located-ness checks of `flat_site_frame_located` and
//! `field_site_chain_trap_ok` were true; the dominance question was never
//! asked of a frame (it was of the by-bci anchors, `note_bci_anchor`).
//!
//! A value is defined at the site when it is a constant or a parameter, or its
//! block is the site's block or dominates it. A frame with an undefined value
//! keeps the rejoining form of the guard, which consults no frame.

use super::Lowerer;
use crate::bailout::{Bailout, BailoutReason};
use crate::ir::{NodeId, Op, NO_NODE};

impl Lowerer<'_> {
    /// Is `node_id`'s value computed before the node being lowered
    /// (`cur_def`) can read it? A value the schedule never placed, an id past
    /// the graph, or a site with no block, is not judged (`true`).
    pub(super) fn frame_value_defined_at_site(&self, node_id: NodeId) -> bool {
        if node_id == NO_NODE {
            return true;
        }
        // An id past the graph is `resolve_frame_state`'s unresumable marker, not ours.
        let Some(node) = self.graph.nodes.get(node_id as usize) else {
            return true;
        };
        if matches!(node.op, Op::Const(_) | Op::ConstF(_) | Op::Param(_) | Op::Dead) {
            return true;
        }
        let blocks = self.schedule.blocks.len();
        let (Some(site), Some(def)) = (
            self.field_proof_block(),
            self.schedule.node_to_block.get(node_id as usize).copied(),
        ) else {
            return true;
        };
        if site >= blocks || def >= blocks {
            return true;
        }
        def == site || self.schedule.dom.dominates(def, site)
    }

    /// The snapshot the deopt pad of the node being lowered publishes (a guard,
    /// `emit_deopt_unless`): the chain snapshot of a chain point, else the
    /// node's own named frame at `bci`, else the flat frame there. The choice
    /// `resolve_frame_state_for_site` / `splice_chain_for_site` make.
    fn site_pad_snapshot(&self, is_chain_point: bool, raw_bci: usize, bci: usize) -> Option<usize> {
        if is_chain_point {
            return self.splice_chain_snapshot_for_site(raw_bci).map(|(si, _)| si);
        }
        self.cur_node_frame
            .map(|si| si as usize)
            .filter(|&si| self.graph.safepoints.get(si).is_some_and(|sp| sp.bci == bci))
            .or_else(|| self.safepoint_first_by_bci.get(&bci).copied())
    }

    /// Does the pad of the node being lowered publish a frame naming a value
    /// its site has not defined? Such a pad (a guard the scheduler or LICM put
    /// in a loop's pre-header with the in-loop frame) rebuilds the interpreter
    /// frame from homes nothing has written yet.
    pub(super) fn site_frame_undefined(&self, is_chain_point: bool, raw_bci: usize, bci: usize) -> bool {
        self.site_pad_snapshot(is_chain_point, raw_bci, bci)
            .is_some_and(|si| !self.snapshot_defined_at_site(si))
    }

    /// Refuse the compile (the single-pass tier takes the method) when the pad
    /// being emitted would publish an undefined frame. This is what the
    /// undominated-anchor refusal caught by accident whenever a guard was the
    /// only reader of a bci: 4 of 21415 guard and `deopt_unless` sites of the
    /// Spring Boot smoke jar (`AbstractImmutableList.hashCode`), the rest of
    /// that refusal's compiles being exception routes' or nobody's.
    pub(super) fn latch_if_site_frame_undefined(&self, is_chain_point: bool, raw_bci: usize, bci: usize) {
        if self.site_frame_undefined(is_chain_point, raw_bci, bci) {
            self.latch_bailout(Bailout::with_context(
                BailoutReason::UnsupportedShape(
                    "ir_lower: a deopt pad's frame names a value its site has not defined",
                ),
                format!("bci {bci} (raw {raw_bci})"),
            ));
        }
    }

    /// [`Self::frame_value_defined_at_site`] for every slot of snapshot `si`.
    pub(super) fn snapshot_defined_at_site(&self, si: usize) -> bool {
        self.graph.safepoints.get(si).is_none_or(|sp| {
            sp.locals
                .iter()
                .chain(sp.stack.iter())
                .chain(sp.monitors.iter())
                .all(|&v| self.frame_value_defined_at_site(v))
        })
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use crate::ir::{CmpOp, Graph, IrType, NodeId, Op, NO_NODE};

    /// `for (i = 0; i < n; i++) ...`: entry block, header (merge, phi, test),
    /// body. Returns `(graph, i's phi, the body's increment, the param)`.
    fn loop_graph() -> (Graph, NodeId, NodeId, NodeId) {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let c0 = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let n = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let region = g.add(Op::Merge, IrType::Control, vec![c0], Some(10));
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let iv = g.add(Op::Phi, IrType::Int, vec![region, zero], Some(10));
        let cond = g.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![iv, n], Some(12));
        let iff = g.add(Op::If, IrType::Control, vec![region, cond], Some(12));
        let exit = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let stay = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        let next = g.add(Op::Add, IrType::Int, vec![iv, one], Some(22));
        g.nodes[iv as usize].inputs.push(next);
        g.nodes[region as usize].inputs.push(stay);
        g.exit = g.add(Op::Return, IrType::Int, vec![exit, iv], None);
        (g, iv, next, n)
    }

    /// A pad whose frame names the loop phi is undefined in the pre-header and
    /// fine in the loop; a chain-less site with no frame at its bci is not judged.
    #[test]
    fn a_pad_naming_a_loop_phi_is_undefined_in_the_pre_header() {
        let (mut g, iv, _next, n) = loop_graph();
        g.push_safepoint(crate::ir::SafepointSnapshot::new(7, vec![iv, n], vec![], vec![]));
        let schedule = crate::ir_schedule::schedule(&g);
        let graph: &'static Graph = Box::leak(Box::new(g));
        let schedule: &'static _ = Box::leak(Box::new(schedule));
        let plan: &'static _ = Box::leak(Box::new(super::super::plan_slots(graph, schedule, None)));
        let mut lo = super::super::tests::lowerer_over(graph, schedule, plan, 4096);
        lo.cur_def = Some(n);
        assert!(lo.site_frame_undefined(false, 7, 7), "the pre-header has not written the phi");
        assert!(!lo.site_frame_undefined(false, 9, 9), "no frame at bci 9: not judged");
        lo.cur_def = Some(iv);
        assert!(!lo.site_frame_undefined(false, 7, 7), "the header defines it");
        lo.cur_def = Some(n);
        lo.latch_if_site_frame_undefined(false, 7, 7);
        assert!(lo.take_latched_bailout().is_some(), "the compile is refused");
    }

    /// A loop phi (and anything the body computes) is not defined in the
    /// pre-header, where LICM puts a hoisted read that keeps its in-loop frame;
    /// it is defined in the loop and after it.
    #[test]
    fn a_loop_phi_is_not_defined_in_the_pre_header_but_is_in_the_loop() {
        let (g, iv, next, n) = loop_graph();
        let schedule = crate::ir_schedule::schedule(&g);
        let graph: &'static Graph = Box::leak(Box::new(g));
        let schedule: &'static _ = Box::leak(Box::new(schedule));
        let plan: &'static _ = Box::leak(Box::new(super::super::plan_slots(graph, schedule, None)));
        let mut lo = super::super::tests::lowerer_over(graph, schedule, plan, 4096);
        let block_of = |id: NodeId| schedule.node_to_block[id as usize];
        assert_ne!(block_of(iv), block_of(n), "the phi is in the header block");
        // The pre-header (entry block): a site whose block the header does not dominate.
        lo.cur_def = Some(n);
        assert!(!lo.frame_value_defined_at_site(iv), "the phi's copy has not run");
        assert!(!lo.frame_value_defined_at_site(next), "nor the body's value");
        assert!(lo.frame_value_defined_at_site(n), "a parameter always is");
        assert!(lo.frame_value_defined_at_site(NO_NODE), "a dead slot always is");
        // The header's own block, and the body the header dominates.
        for site in [iv, next] {
            lo.cur_def = Some(site);
            assert!(lo.frame_value_defined_at_site(iv), "site n{site}");
        }
    }
}
