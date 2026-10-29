// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 4, lane IRROWS (IRLOWER17-3 of the r18 backlog,
//! `docs/known-issues/jit/j27-r18-backlog-live-rows-20261028.md`): the by-bci
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
//! * `CRATONVM_JIT_IR_REFUSE_UNDOMINATED_ANCHOR=1` (opt-in): refuse the
//!   compile when an undominated anchor sits at a bci where a transfer reads
//!   the FLAT snapshot (`Lowerer::by_bci_transfer_bcis`), the only bcis whose
//!   by-bci point a resume can consult. The single-pass tier takes the method.
//!
//! Opt-in because the census has never been taken: the refusal is fail-closed
//! (a refused compile, never a wrong value), but its reach is unknown.

use super::Lowerer;
use crate::bailout::{Bailout, BailoutReason};
use std::collections::{HashMap, HashSet};

/// `CRATONVM_JIT_IR_REFUSE_UNDOMINATED_ANCHOR=1` (j27 wave 4, lane IRROWS;
/// opt-in, inert with `CRATONVM_JIT_IR_BCI_ANCHOR_DOMINATED=0`, which records
/// no standing). Read once per lowering; no `static`.
pub(super) fn refuse_undominated_anchor_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_REFUSE_UNDOMINATED_ANCHOR")
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
        let found = undominated_anchors(&self.bci_native_dominated, &self.by_bci_transfer_bcis);
        if found.bcis.is_empty() {
            return;
        }
        if dbg {
            eprintln!(
                "[ir] bci anchors undominated={} flat_transfer={} first_bci={}",
                found.bcis.len(),
                found.flat_transfer.len(),
                found.bcis[0],
            );
        }
        if let Some(bailout) = undominated_anchor_refusal(&found, refuse_undominated_anchor_enabled())
        {
            self.latch_bailout(bailout);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn standing(rows: &[(usize, bool)]) -> HashMap<usize, bool> {
        rows.iter().copied().collect()
    }

    fn set(bcis: &[usize]) -> HashSet<usize> {
        bcis.iter().copied().collect()
    }

    #[test]
    fn j27_irrows_only_undominated_anchors_are_counted_in_bci_order() {
        let found = undominated_anchors(
            &standing(&[(9, false), (3, true), (5, false), (7, true)]),
            &set(&[3, 9]),
        );
        assert_eq!(found.bcis, vec![5, 9]);
        assert_eq!(found.flat_transfer, vec![9], "bci 3 transfers but its anchor is honest");
    }

    #[test]
    fn j27_irrows_every_honest_anchor_counts_nothing() {
        let found = undominated_anchors(&standing(&[(1, true), (4, true)]), &set(&[1, 4]));
        assert_eq!(found, UndominatedAnchors::default());
    }

    #[test]
    fn j27_irrows_the_refusal_needs_the_switch_and_a_flat_transfer() {
        let found = undominated_anchors(&standing(&[(5, false), (9, false)]), &set(&[9]));
        assert!(undominated_anchor_refusal(&found, false).is_none(), "opt-in");
        let bailout = undominated_anchor_refusal(&found, true).expect("bci 9 is read by a transfer");
        assert!(
            matches!(bailout.reason, BailoutReason::UnsupportedShape(_)),
            "{bailout:?}"
        );
        assert!(format!("{bailout:?}").contains("bci 9"), "{bailout:?}");
        // An undominated anchor no transfer reads is counted, never refused.
        let unread = undominated_anchors(&standing(&[(5, false)]), &set(&[9]));
        assert_eq!(unread.bcis, vec![5]);
        assert!(undominated_anchor_refusal(&unread, true).is_none());
    }
}
