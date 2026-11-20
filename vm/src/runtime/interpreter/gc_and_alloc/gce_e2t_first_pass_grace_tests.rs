use super::uncounted_roster;

/// Once every counted mutator has arrived, the pass owes a signal only to
/// the roster threads the census did not count (it counted blocked ones
/// too), in roster order.
#[test]
fn the_grace_pass_signals_only_the_uncounted_roster() {
    assert_eq!(uncounted_roster(&[5, 9, 11, 14], &[9, 5, 14]), vec![11]);
    assert!(uncounted_roster(&[5, 9], &[9, 5, 77]).is_empty());
    assert_eq!(uncounted_roster(&[3, 2], &[]), vec![3, 2]);
    assert!(uncounted_roster(&[], &[1]).is_empty());
}

/// The loop's early exit sits at the top of the loop, so a met grace runs
/// no round and the unmet case runs every round it did before.
#[test]
fn a_met_grace_skips_the_round_loop() {
    let src = super::GC_AND_ALLOC_SRC;
    let start = src
        .find("fn stw_take_over_and_wait(")
        .expect("the take-over");
    let b = &src[start..];
    let grace = b.find("let grace_met =").expect("grace");
    // The early exit is the `if grace_met {` whose body is `break;`.
    let open = "if grace_met {";
    let exit = b
        .match_indices(open)
        .map(|(i, _)| i)
        .find(|&i| b[i + open.len()..].trim_start().starts_with("break;"))
        .expect("early exit");
    let rounds = b.find("stw_takeover_should_scan(rounds").expect("rounds");
    assert!(grace < exit && exit < rounds);
}
