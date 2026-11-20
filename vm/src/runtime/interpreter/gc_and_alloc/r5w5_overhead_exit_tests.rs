//! gen r5w5/sizer9: the overhead-limit arm's exit.
use super::*;

/// gen r4w5/thrash5's guarantee is kept: a full collection this call ran
/// that leaves the streak latched is the error, attempted or not.
#[test]
fn a_major_that_leaves_the_streak_latched_still_throws() {
    for soft in [false, true] {
        assert_eq!(
            overhead_limit_exit(true, true, soft, true),
            OverheadLimitExit::Throw
        );
    }
}

/// A major that reset the streak, or a soft-reference collection, earns
/// the one attempt it always did.
#[test]
fn a_productive_collection_earns_the_attempt() {
    assert_eq!(
        overhead_limit_exit(true, true, false, false),
        OverheadLimitExit::Attempt
    );
    assert_eq!(
        overhead_limit_exit(true, false, true, false),
        OverheadLimitExit::Attempt
    );
}

/// No collection on this thread in a judged arm: the verdict is not
/// honoured on young cycles alone — finish the ladder.
#[test]
fn no_full_collection_means_no_verdict() {
    assert_eq!(
        overhead_limit_exit(true, false, false, false),
        OverheadLimitExit::AttemptThenLastDitch
    );
}

/// `CRATONVM_GC_OVERHEAD_PROGRESS=0` (and every non-Generational heap):
/// the historical exit, whatever ran.
#[test]
fn the_unjudged_arm_is_unchanged() {
    for (major, soft, latched) in [
        (false, false, false),
        (true, false, true),
        (false, true, false),
    ] {
        assert_eq!(
            overhead_limit_exit(false, major, soft, latched),
            OverheadLimitExit::Attempt
        );
    }
}
