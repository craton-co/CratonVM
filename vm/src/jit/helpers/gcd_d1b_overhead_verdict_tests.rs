//! gcd d1/b: the latched overhead limit of a compiled allocation throws
//! only after a full collection this thread ran.
use super::jit_overhead_limit_verdict as verdict;

/// Off the gate (G1, ZGC, `CRATONVM_GC_OVERHEAD_PROGRESS=0`): the
/// historical answer, whatever the streak says.
#[test]
fn the_unjudged_answer_is_unchanged() {
    for latched in [false, true] {
        assert!(verdict(false, false, false, latched));
        assert!(!verdict(false, true, false, latched));
    }
}

/// A soft-reference collection is judged as before (gen r5w2/oomjit6).
#[test]
fn a_soft_collection_still_decides_by_the_streak() {
    assert!(verdict(true, true, false, true));
    assert!(!verdict(true, true, false, false));
}

/// The fix: no soft collection used to be an immediate throw. Now the
/// major decides -- and a major this thread did not run decides nothing.
#[test]
fn no_soft_collection_needs_a_major_before_the_error() {
    assert!(
        verdict(true, false, true, true),
        "a wedged heap still throws"
    );
    assert!(
        !verdict(true, false, true, false),
        "a productive major earns the retry"
    );
    assert!(
        !verdict(true, false, false, true),
        "a lost STW race is no verdict"
    );
}
