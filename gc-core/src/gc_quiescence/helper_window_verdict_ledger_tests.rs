use super::*;
use std::sync::Arc;

/// gcd d2/i: VM A's pass recorded an unpinned window; VM B's take-over,
/// which found none, records 0 in ITS ledger. A's bound read still says
/// "one refuses" -- it used to read B's 0 through the process static.
#[test]
fn another_vms_zero_does_not_overwrite_this_pauses_refusal() {
    let a = Arc::new(PauseLedger::new());
    let b = Arc::new(PauseLedger::new());
    with_pause_ledger(&a, || set_xt_helper_windows_unpinned(1));
    let b2 = Arc::clone(&b);
    std::thread::spawn(move || {
        with_pause_ledger(&b2, || {
            reset_xt_cycle();
            set_xt_helper_windows_unpinned(0);
        })
    })
    .join()
    .unwrap();
    assert_eq!(with_pause_ledger(&a, xt_helper_windows_unpinned), Some(1));
    assert_eq!(with_pause_ledger(&b, xt_helper_windows_unpinned), Some(0));
}

/// The row is per pause: the take-over's `reset_xt_cycle` and the pause
/// opener both clear it, so a pause that runs no pass reads 0, not the
/// previous pause's refusal.
#[test]
fn the_refusal_is_cleared_by_the_take_over_reset_and_by_the_pause_open() {
    let l = Arc::new(PauseLedger::new());
    with_pause_ledger(&l, || {
        set_xt_helper_windows_unpinned(3);
        reset_xt_cycle();
        assert_eq!(xt_helper_windows_unpinned(), Some(0));
        set_xt_helper_windows_unpinned(2);
        reset_peer_proven_jit_depth();
        assert_eq!(xt_helper_windows_unpinned(), Some(0));
    });
}
