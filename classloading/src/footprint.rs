// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! zs s1/fp (2026-10-14): size estimates for the per-class side tables'
//! footprint census ([`crate::ClassManager::footprint_census`],
//! [`crate::ClassStore::footprint_census`]). Diagnostics only: nothing on a
//! load or lookup path reads these.

/// Estimated bytes of a hashbrown table that reports `capacity` usable slots
/// of `entry` bytes: the buckets are the next power of two at or above `8/7`
/// of the capacity (at least 4 once allocated), each costing the entry plus
/// one control byte, plus one 16-byte group of trailing control bytes. Zero
/// for a table that never allocated. The same rule as the GC's
/// `zgc_fp_hash_table_bytes`, plus the trailing group.
pub(crate) fn hash_table_bytes(capacity: usize, entry: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    let buckets = capacity
        .saturating_mul(8)
        .div_ceil(7)
        .max(4)
        .checked_next_power_of_two()
        .unwrap_or(usize::MAX);
    buckets
        .saturating_mul(entry.saturating_add(1))
        .saturating_add(16)
}

#[cfg(test)]
mod tests;
