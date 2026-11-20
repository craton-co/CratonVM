// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave 7: one VM-wide layout-epoch proof instead of a compare per loaded
//! reference.
//!
//! `coestablish_loaded_reference` puts a compactness proof on a reference
//! loaded inside a loop (`p = p.next`) and, beside it, a deopting
//! layout-epoch compare. When the entry / pre-header epoch guard already
//! dominates the definition ([`Lowerer::epoch_proof_holds`]), every field site
//! on that value elides its own epoch compare through the same proof
//! (`emit_field_layout_guard`), so the per-definition compare only repeats the
//! entry's check once per trip. It is kept for a value some other kind of
//! reader consumes (a collection or string prefix reads the value-keyed
//! `layout_proven` row), because those readers do not consult the VM-wide
//! proof.
//!
//! Skipping the compare is never a correctness question: every field site
//! still asks `epoch_proof_holds` for its own block and emits its own guard
//! when the answer there is no.

use super::*;

impl Lowerer<'_> {
    /// May `coestablish_loaded_reference` leave out its epoch compare for
    /// `value`, defined in `block`?
    pub(in crate::ir_lower) fn coestablish_epoch_covered(
        &self,
        value: NodeId,
        block: usize,
    ) -> bool {
        value_read_only_as_field_receiver(self.graph, value) && self.epoch_proof_holds(block)
    }
}

/// Is every reader of `value` a field access that uses it as its receiver
/// (input 2 of an `Op::Load` / `Op::Store`), a reference compare (which never
/// dereferences), or a phi whose readers are again only those? Pure; walks
/// the graph's users through phis once each.
pub(in crate::ir_lower) fn value_read_only_as_field_receiver(graph: &Graph, value: NodeId) -> bool {
    let mut seen: std::collections::HashSet<NodeId> = std::collections::HashSet::new();
    let mut work = vec![value];
    seen.insert(value);
    while let Some(v) = work.pop() {
        for (id, n) in graph.nodes.iter().enumerate() {
            let uses = n.inputs.iter().filter(|&&i| i == v).count();
            if uses == 0 {
                continue;
            }
            match n.op {
                Op::Load(_) if uses == 1 && n.inputs.get(2) == Some(&v) => {}
                // A store of `v` itself (input 4) publishes it to readers this
                // walk cannot see.
                Op::Store(_) if uses == 1 && n.inputs.get(2) == Some(&v) => {}
                Op::Cmp(_) => {}
                Op::Phi => {
                    // Cast: an index into the node arena is a `NodeId`.
                    let phi = id as NodeId;
                    if seen.insert(phi) {
                        work.push(phi);
                    }
                }
                _ => return false,
            }
        }
    }
    true
}
