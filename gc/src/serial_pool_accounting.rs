// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Logical Serial pool accounting over the generational collector's arenas.
//!
//! An arena's address reservation is not a pool maximum. In particular, a
//! pinned young survivor can remain in from-space after its logical survivor
//! budget is full, and old storage can borrow the young generation's budget.
//! Account for those bytes without enlarging the fixed pool maxima or losing
//! live bytes. Collector workspace committed beyond the allocatable heap is
//! not Java heap commitment; OS commitment remains a separate measurement.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PoolUsage {
    pub used: usize,
    pub committed: usize,
    pub max: usize,
}

/// Returns Eden, Survivor and Tenured usage with exact fixed maxima.
///
/// Every live byte is retained in the accounting. `None` means the inputs
/// cannot describe a coherent heap within those maxima; callers must obtain
/// a coherent snapshot rather than clamp away live bytes.
pub(crate) fn project(
    maxima: [usize; 3],
    used: [usize; 2],
    committed: [usize; 2],
    young_survivors: usize,
    survivor_ratio: usize,
    alignment: usize,
) -> Option<[PoolUsage; 3]> {
    let [eden_max, survivor_max, tenured_max] = maxima;
    let young_max = eden_max.checked_add(survivor_max)?;
    let total_max = young_max.checked_add(tenured_max)?;
    let [young_used, old_used] = used;
    let total_used = young_used.checked_add(old_used)?;
    if total_used > total_max {
        return None;
    }
    let survivors = young_survivors.min(young_used);
    // Logical promotion need not move a pinned object's physical address.
    let promoted = survivors.saturating_sub(survivor_max);
    let tenured_used = old_used
        .saturating_add(promoted)
        .min(tenured_max)
        .max(total_used.saturating_sub(young_max));
    let young_used = total_used - tenured_used;
    let survivor_used = survivors
        .min(survivor_max)
        .min(young_used)
        .max(young_used.saturating_sub(eden_max));
    let eden_used = young_used - survivor_used;

    let total_committed = committed[0]
        .saturating_add(committed[1])
        .max(total_used)
        .min(total_max);
    let tenured_low = tenured_used.max(total_committed.saturating_sub(young_max));
    let tenured_high = tenured_max.min(total_committed - young_used);
    let tenured_committed = committed[1]
        .saturating_add(promoted)
        .clamp(tenured_low, tenured_high);
    let young_committed = total_committed - tenured_committed;
    let survivor_low = survivor_used.max(young_committed.saturating_sub(eden_max));
    let survivor_high = survivor_max.min(young_committed - eden_used);
    let reserve = young_committed / survivor_ratio.max(1).saturating_add(1);
    let align = alignment.max(1);
    let survivor_committed = (reserve / align * align).clamp(survivor_low, survivor_high);
    Some([
        PoolUsage {
            used: eden_used,
            committed: young_committed - survivor_committed,
            max: eden_max,
        },
        PoolUsage {
            used: survivor_used,
            committed: survivor_committed,
            max: survivor_max,
        },
        PoolUsage {
            used: tenured_used,
            committed: tenured_committed,
            max: tenured_max,
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn survivor_overflow_preserves_live_bytes_and_fixed_maxima() {
        let maxima = [71_630_848, 8_912_896, 178_978_816];
        let young = 35_652_128;
        let old = 130_000_000;
        let pools = project(maxima, [young, old], [64 << 20, 170 << 20], young, 8, 65536).unwrap();
        assert_eq!(pools.map(|p| p.max), maxima);
        assert_eq!(pools.iter().map(|p| p.used).sum::<usize>(), young + old);
        assert_eq!(pools[1].used, maxima[1]);
        assert_eq!(pools[2].used, old + young - maxima[1]);
    }

    #[test]
    fn every_representable_small_snapshot_preserves_usage_and_commit_invariants() {
        for eden in 0..=5 {
            for survivor in 0..=3 {
                for old in 0..=5 {
                    let maxima = [eden, survivor, old];
                    let max = eden + survivor + old;
                    for young_used in 0..=max {
                        for old_used in 0..=max - young_used {
                            for retained in [0, young_used / 2, young_used] {
                                for commitment in [[0, 0], [young_used, old_used], [max, max]] {
                                    let pools = project(
                                        maxima,
                                        [young_used, old_used],
                                        commitment,
                                        retained,
                                        8,
                                        1,
                                    )
                                    .unwrap();
                                    assert_eq!(pools.map(|p| p.max), maxima);
                                    assert_eq!(
                                        pools.iter().map(|p| p.used).sum::<usize>(),
                                        young_used + old_used
                                    );
                                    let expected = commitment[0]
                                        .saturating_add(commitment[1])
                                        .max(young_used + old_used)
                                        .min(max);
                                    assert_eq!(
                                        pools.iter().map(|p| p.committed).sum::<usize>(),
                                        expected
                                    );
                                    assert!(pools
                                        .iter()
                                        .all(|p| p.used <= p.committed && p.committed <= p.max));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn incoherent_over_budget_inputs_are_refused_without_hiding_live_bytes() {
        assert!(project([8, 1, 16], [10, 16], [10, 16], 10, 8, 1).is_none());
        assert!(project([usize::MAX, 1, 0], [0, 0], [0, 0], 0, 8, 1).is_none());
    }
}
