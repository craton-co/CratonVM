// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 6 (lane T1LOOPSTATIC, LOOPSTATIC21-3 of
//! `docs/feature-designs/jit-ir-optimizations.md`): one
//! `CRATONVM_DBG_UNROLL` line per compile whose loop promotions took a loop,
//! saying whether the post-promotion unroll (TYPECHECK21-6,
//! `CRATONVM_JIT_IR_UNROLL_PROMOTED_LOOPS`) is offered the graph and, when
//! not, which gate kept it. When it IS offered, the unroller's own `bail` /
//! `unrolled` lines follow and name its answer. Diagnostic only: it reads the
//! gates `optimize_passes` already computed and changes nothing. The unroller
//! itself is the loops lane's; this module only reports what reaches it.

use crate::ir::Graph;

/// Why the post-promotion unroll is not offered a graph whose promotions took
/// a loop, or `None` when it is (or when nothing was promoted). `unroll_on`
/// is `ir_optimize::unroll_enabled()`, `may_unroll` the loop rounds' latch
/// after they ran, `switch_on` `CRATONVM_JIT_IR_UNROLL_PROMOTED_LOOPS`.
pub(super) fn not_offered_reason(
    promoted_cells: bool,
    unroll_on: bool,
    may_unroll: bool,
    switch_on: bool,
) -> Option<&'static str> {
    if !promoted_cells {
        None
    } else if !unroll_on {
        Some("the unroller is off (CRATONVM_JIT_UNROLL=0)")
    } else if !may_unroll {
        Some("the latch is spent: the loop rounds already unrolled a loop of this method")
    } else if !switch_on {
        Some("CRATONVM_JIT_IR_UNROLL_PROMOTED_LOOPS is off")
    } else {
        None
    }
}

/// Is the census line wanted (`CRATONVM_DBG_UNROLL=1`)? Read without a
/// `static`.
pub(super) fn enabled() -> bool {
    false}

/// Print the census line for a graph whose promotions took a loop.
pub(super) fn note(graph: &Graph, unroll_on: bool, may_unroll: bool, switch_on: bool) {
    let bcis = &graph.promoted_cell_header_bcis;
    match not_offered_reason(true, unroll_on, may_unroll, switch_on) {
        Some(why) => eprintln!(
            "[DBG_UNROLL] {}: promoted header bci(s) {bcis:?} not offered to the unroller -- {why}",
            graph.scope_method_key
        ),
        None => eprintln!(
            "[DBG_UNROLL] {}: promoted header bci(s) {bcis:?} offered to the unroller",
            graph.scope_method_key
        ),
    }
}

#[cfg(test)]
mod tests;
