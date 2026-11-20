use super::*;

/// gc-common w4-a: the post-quota take-over time is per cycle — published
/// once by the take-over, read by the pause line, and zeroed by the next
/// cycle's `reset_xt_cycle` so a pause with no take-over reads 0, not the
/// previous pause's figure.
#[test]
fn the_post_quota_time_is_reset_with_the_cycle() {
    publish_xt_post_quota_ns(12_345);
    assert_eq!(xt_post_quota_ns(), 12_345);
    // gc-common w6-f: the passes' share of `ttsp_us` accumulates per pass
    // and is zeroed with the cycle.
    add_xt_pass_ns(1_000);
    add_xt_pass_ns(500);
    assert_eq!(xt_pass_ns(), 1_500);
    reset_xt_cycle();
    assert_eq!(xt_post_quota_ns(), 0);
    assert_eq!(xt_pass_ns(), 0);
}
