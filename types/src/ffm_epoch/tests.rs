use super::*;

#[test]
fn the_epoch_starts_non_zero_and_advances() {
    // Non-zero start is load-bearing: a verdict struct that was never
    // published reads as all-zero, and must not compare equal to a live
    // epoch.
    assert!(ffm_epoch() >= 1);
    let before = ffm_epoch();
    bump_ffm_epoch();
    assert!(
        ffm_epoch() > before,
        "a bump must make every prior verdict stale"
    );
}
