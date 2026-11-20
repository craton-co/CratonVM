use super::{next_young_trigger, GenerationalHeap, TriggerFeedback};

const M: usize = 1 << 20;

/// The in-band arm, off (historical): the trial is dropped and the next
/// over-goal pause halves AGAIN -- vi1-yg `yg1_cost_yp_ck_3`'s
/// 203 / 200 / 205 ms. On: the 200 ms pause judges the trial (not a
/// quarter below 203), reverts to 64 MB and latches, and the 205 ms pause
/// is a latched hold.
#[test]
fn gci_i2yg_an_in_band_pause_judges_a_pending_trial() {
    let (floor, ceiling, cap) = (8 * M, 64 * M, 128 * M);
    let seq = |judge: bool| {
        let mut fb = TriggerFeedback {
            judge_in_band: judge,
            ..TriggerFeedback::default()
        };
        let t1 = next_young_trigger(&mut fb, 64 * M, floor, ceiling, cap, 203, 200, (77 * M) as u64);
        let t2 = next_young_trigger(&mut fb, t1, floor, ceiling, cap, 200, 200, (78 * M) as u64);
        let t3 = next_young_trigger(&mut fb, t2, floor, ceiling, cap, 205, 200, (79 * M) as u64);
        (t1, t2, t3, fb.census.halvings, fb.census.reverts, fb.census.latched_holds)
    };
    assert_eq!(seq(false), (32 * M, 32 * M, 16 * M, 2, 0, 0), "historical arm");
    assert_eq!(seq(true), (32 * M, 64 * M, 64 * M, 1, 1, 1), "judged in band");

    // A halving that DID help (300 -> 150 ms, more than a quarter) is
    // kept and its trial closed: the next over-goal pause opens a fresh
    // trial rather than judging a stale one.
    let mut fb = TriggerFeedback {
        judge_in_band: true,
        ..TriggerFeedback::default()
    };
    let t1 = next_young_trigger(&mut fb, 64 * M, floor, ceiling, cap, 300, 200, (10 * M) as u64);
    let t2 = next_young_trigger(&mut fb, t1, floor, ceiling, cap, 150, 200, (10 * M) as u64);
    assert_eq!((t1, t2, fb.census.reverts), (32 * M, 32 * M, 0));
    assert!(fb.trial.is_none() && fb.latched_copied.is_none());
    let t3 = next_young_trigger(&mut fb, t2, floor, ceiling, cap, 210, 200, (10 * M) as u64);
    assert_eq!((t3, fb.census.halvings), (16 * M, 2));    }