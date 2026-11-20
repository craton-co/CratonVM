// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Non-null and known-length helpers for `Op::IndyRootRef` and trusted static
//! final arrays in `ir_check_elim`.

use crate::ir::{Graph, IrType, Node, NodeId, Op};
use rustc_hash::FxHashMap;

/// Collect known lengths `(node_id -> length)` for trusted `static final`
/// arrays loaded via either non-volatile `Op::LoadStatic` or `Op::IndyRootRef`.
pub(super) fn trusted_static_arrays_with_root_refs(graph: &Graph) -> FxHashMap<NodeId, i32> {
    if !crate::ir_optimize::ir_static_final_array_length_enabled() {
        return FxHashMap::default();
    }
    if graph.trusted_static_array_lengths.is_empty() && graph.const_ref_meta.is_empty() {
        return FxHashMap::default();
    }
    graph
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(id, node)| {
            if node.volatile_access || node.ty != IrType::Ref {
                return None;
            }
            let length = match node.op {
                Op::LoadStatic {
                    class_id,
                    field_index,
                    is_volatile: false,
                    ..
                } => graph
                    .trusted_static_array_lengths
                    .iter()
                    .find(|&&(key, _)| key == (class_id, field_index))
                    .map(|&(_, len)| len)?,
                Op::IndyRootRef { slot_addr } => {
                    let meta = graph.const_ref_meta.get(&slot_addr)?;
                    meta.array_length.or_else(|| {
                        let key = meta.static_field?;
                        graph
                            .trusted_static_array_lengths
                            .iter()
                            .find(|&&(k, _)| k == key)
                            .map(|&(_, len)| len)
                    })?
                }
                _ => return None,
            };
            (length >= 0).then_some((NodeId::try_from(id).ok()?, length))
        })
        .collect()
}

/// True when `node` is an `Op::IndyRootRef` or trusted `Op::LoadStatic` known
/// non-null in `Graph` metadata.
pub(super) fn is_non_null_root_or_static(graph: &Graph, node: &Node) -> bool {
    match node.op {
        Op::IndyRootRef { slot_addr } => graph
            .const_ref_meta
            .get(&slot_addr)
            .is_some_and(|m| m.non_null),
        Op::LoadStatic {
            class_id,
            field_index,
            is_volatile: false,
            ..
        } if !node.volatile_access => graph
            .static_final_non_null
            .contains(&(class_id, field_index)),
        _ => false,
    }
}
