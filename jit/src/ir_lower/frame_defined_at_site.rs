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
mod tests;
