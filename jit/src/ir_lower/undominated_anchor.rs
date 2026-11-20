// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 4, lane IRROWS (IRLOWER17-3 of the r18 backlog,
//! `docs/feature-designs/jit-ir-optimizations.md`): the by-bci
//! table anchors that stayed UNDOMINATED.
//!
//! `Lowerer::note_bci_anchor` (`CRATONVM_JIT_IR_BCI_ANCHOR_DOMINATED`, default
//! ON) prefers, per bci, an owner whose block every block defining a value of
//! that bci's by-bci snapshot dominates, and records in
//! `Lowerer::bci_native_dominated` whether the owner it kept is one. When
//! every owner of a bci is undominated, the table point `build_deopt_points`
//! anchors there describes values that the path through the anchor never
//! computed. The lowering recorded that fact and nothing read it: no census
//! counted it and no check refused it (`ir_verify` runs on the graph, before
//! any anchor exists, so it cannot).
//!
//! This module reads it, in two ways:
//!
//! * a census line under `CRATONVM_DBG_IR_COMPILES` / `CRATONVM_DBG_JITC`,
//!   printed for a lowering with at least one undominated anchor:
//!   `[ir] bci anchors undominated=N flat_transfer=K first_bci=B`;
//! * `CRATONVM_JIT_IR_REFUSE_UNDOMINATED_ANCHOR` (default ON, `=0` kills): refuse the
//!   compile when an undominated anchor sits at a bci where a transfer resumes
//!   through the by-bci TABLE point (`Lowerer::table_transfer_bcis`: exception
//!   routes, OSR re-checks). A guard bakes its own boxed frame and never reads
//!   the table point; its frame is held to `latch_if_site_frame_undefined`.
//!   The single-pass tier takes the method.
//!
//! Default ON because the census was taken (2026-11-02, Spring Boot smoke: 19
//! methods with a flat transfer on an undominated anchor) and the forced-chain
//! soak (`CRATONVM_DEOPT_EAGER_CHAINS=1`) gave a wrong array index in
//! `ZipContent$Loader.sort` with the refusal off and HotSpot's answer with it
//! on. The refusal is fail-closed: a refused compile, never a wrong value.

use super::Lowerer;
use crate::bailout::{Bailout, BailoutReason};
use std::collections::{HashMap, HashSet};

/// `CRATONVM_JIT_IR_REFUSE_UNDOMINATED_ANCHOR` (j27 wave 4, lane IRROWS;
/// default ON since 2026-11-02, `=0` kills; inert with
/// `CRATONVM_JIT_IR_BCI_ANCHOR_DOMINATED=0`, which records no standing). Read
/// once per lowering; no `static`.
pub(super) fn refuse_undominated_anchor_enabled() -> bool {
    // WARNING j27-user-flip-20261102 (default ON; =0 restores the old default)
    true
}

/// What the anchors of one lowering say.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct UndominatedAnchors {
    /// The bcis whose kept anchor is undominated, ascending.
    pub(super) bcis: Vec<usize>,
    /// Those of them at which a transfer reads the flat snapshot, ascending.
    pub(super) flat_transfer: Vec<usize>,
}

/// Classify `dominated` (bci -> the kept anchor's standing) against the bcis
/// whose flat snapshot a transfer reads.
pub(super) fn undominated_anchors(
    dominated: &HashMap<usize, bool>,
    flat_transfer_bcis: &HashSet<usize>,
) -> UndominatedAnchors {
    let mut bcis: Vec<usize> = dominated
        .iter()
        .filter(|&(_, &d)| !d)
        .map(|(&bci, _)| bci)
        .collect();
    bcis.sort_unstable();
    let flat_transfer = bcis
        .iter()
        .copied()
        .filter(|bci| flat_transfer_bcis.contains(bci))
        .collect();
    UndominatedAnchors {
        bcis,
        flat_transfer,
    }
}

/// The refusal for `found`, when the switch is on and a transfer reads an
/// undominated anchor's point; `None` otherwise.
pub(super) fn undominated_anchor_refusal(
    found: &UndominatedAnchors,
    refuse_on: bool,
) -> Option<Bailout> {
    let &bci = found.flat_transfer.first().filter(|_| refuse_on)?;
    Some(Bailout::with_context(
        BailoutReason::UnsupportedShape(
            "ir_lower: a by-bci table point anchors where its values were not computed",
        ),
        format!(
            "bci {bci}: no owner sits in a block its snapshot's defining blocks dominate \
             ({} undominated anchor(s))",
            found.bcis.len()
        ),
    ))
}

impl Lowerer<'_> {
    /// Census and (opt-in) refusal of the undominated by-bci anchors. Called
    /// once, after `build_deopt_points` and before the latch's second read, so
    /// a refusal here takes the same exit as every other late latch.
    pub(super) fn check_undominated_anchors(&self, dbg: bool) {
        if self.bci_native_dominated.is_empty() {
            return;
        }
        let found = undominated_anchors(&self.bci_native_dominated, &self.table_transfer_bcis);
        if found.bcis.is_empty() {
            return;
        }
        if dbg {
            eprintln!(
                "[ir] bci anchors undominated={} flat_transfer={} first_bci={} method={}",
                found.bcis.len(),
                found.flat_transfer.len(),
                found.bcis[0],
                self.graph.dbg_label,
            );
            for &bci in &found.flat_transfer {
                let first = self.safepoint_first_by_bci.get(&bci).copied();
                let claimed = first.is_some_and(|si| self.snapshot_native.contains_key(&(si as u32)));
                let owners = self
                    .graph
                    .nodes
                    .iter()
                    .filter(|n| n.op != crate::ir::Op::Dead && n.bytecode_pc == Some(bci))
                    .count();
                let owner = self
                    .graph
                    .nodes
                    .iter()
                    .enumerate()
                    .find(|(_, n)| n.op != crate::ir::Op::Dead && n.bytecode_pc == Some(bci));
                let owner_info = owner.map(|(id, n)| {
                    let blk = self.schedule.node_to_block.get(id).copied();
                    let bad: Vec<String> = first
                        .and_then(|si| self.graph.safepoints.get(si))
                        .map(|sp| {
                            sp.locals
                                .iter()
                                .chain(sp.stack.iter())
                                .copied()
                                .filter(|&v| v != crate::ir::NO_NODE)
                                .filter_map(|v| {
                                    let vn = self.graph.nodes.get(v as usize)?;
                                    let at = if matches!(vn.op, crate::ir::Op::Phi) {
                                        vn.input_opt(0).unwrap_or(crate::ir::NO_NODE)
                                    } else {
                                        v
                                    };
                                    let vb = self.schedule.node_to_block.get(at as usize).copied()?;
                                    let ok = blk.is_some_and(|b| vb == b || self.schedule.dom.dominates(vb, b));
                                    (!ok).then(|| format!("n{v}:{:?}@b{vb}", vn.op))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    format!("owner=n{id}:{:?} ctrl={:?} block={blk:?} undefined=[{}]", n.op, n.inputs.first(), bad.join(","))
                });
                eprintln!(
                    "[ir] bci anchor detail bci={bci} first_snapshot={first:?} claimed={claimed} owners={owners} why={:?} {} method={}",
                    self.by_bci_transfer_why.get(&bci),
                    owner_info.unwrap_or_default(),
                    self.graph.dbg_label,
                );
            }
        }
        if let Some(bailout) = undominated_anchor_refusal(&found, refuse_undominated_anchor_enabled())
        {
            self.latch_bailout(bailout);
        }
    }
}

#[cfg(test)]
mod tests;
