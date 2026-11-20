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
