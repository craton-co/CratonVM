// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 8 (lane W8LOOP; race B of
//! `docs/feature-designs/jit-ir-optimizations.md`):
//! how full the branch rows were when a compile read them.
//!
//! Branch recording is interpreter-only (`profile::BranchCounters`), so a
//! kernel that a fast C1 worker or an early OSR body takes out of the
//! interpreter reaches C2 with rows below [`crate::ir::MIN_OBSERVATIONS_TO_PRUNE`]
//! and gets no never-taken prunes. The existing `[ir] never-taken feed` line
//! prints only when something is offered, so exactly the thin-row compiles
//! race B is about printed nothing. This census prints for every compile that
//! has branch rows, under the same channels (`CRATONVM_DBG_IR_COMPILES` /
//! `CRATONVM_DBG_IR_BAILOUT`). Debug output only; no decision reads it.

use crate::profile::BranchCounts;
use rustc_hash::FxHashMap;

/// The fill of one method's branch rows at compile time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RowCensus {
    /// Profiled branch pcs.
    pub(crate) rows: usize,
    /// Rows whose two directions together reach the prune floor.
    pub(crate) full: usize,
    /// The largest `taken + not_taken` of any row.
    pub(crate) max_total: u64,
}

/// Count the rows of `branches` against the prune floor. Pure.
pub(crate) fn row_census(branches: &FxHashMap<usize, BranchCounts>) -> RowCensus {
    let floor = u64::from(crate::ir::MIN_OBSERVATIONS_TO_PRUNE);
    branches.values().fold(RowCensus::default(), |mut c, b| {
        let total = u64::from(b.taken) + u64::from(b.not_taken);
        c.rows += 1;
        c.full += usize::from(total >= floor);
        c.max_total = c.max_total.max(total);
        c
    })
}

/// The census line (`grep 'never-taken rows'`).
pub(crate) fn census_line(method_key: &str, census: RowCensus, offered: usize) -> String {
    format!(
        "[ir] never-taken rows in {method_key}: rows={} full={} (floor {}) max_total={} offered={offered}",
        census.rows,
        census.full,
        crate::ir::MIN_OBSERVATIONS_TO_PRUNE,
        census.max_total
    )
}

#[cfg(test)]
mod tests;
