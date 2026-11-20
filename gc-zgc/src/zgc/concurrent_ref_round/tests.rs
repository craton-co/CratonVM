use super::*;
use crate::zgc::ZgcRealHeap;


/// Off, or with no deferred sweep, nothing is asked or counted; the hold
/// makes another runner wait (bounded for an allocation) and the release
/// books the round.
#[test]
fn zt31zc11_the_hold_and_its_census() {
    let heap = ZgcRealHeap::with_capacity(16 * 1024 * 1024);
    heap.counters.crround.set_enabled_for_test(false);
    assert!(!heap.reference_round_may_follow_release());
    heap.counters.crround.set_enabled_for_test(true);
    assert!(!heap.reference_round_may_follow_release(), "no sweep parked");
    assert_eq!(heap.concurrent_ref_round_report().refused, 0);
    assert!(heap.wait_for_sweep_install_hold(None), "no hold: no wait");
    heap.hold_sweep_install();
    assert!(
        !heap.wait_for_sweep_install_hold(Some(std::time::Duration::from_millis(5))),
        "a bounded wait gives up"
    );
    std::thread::scope(|s| {
        let waiter = s.spawn(|| heap.wait_for_sweep_install_hold(None));
        std::thread::sleep(std::time::Duration::from_millis(5));
        heap.release_sweep_install_hold(1_234);
        assert!(waiter.join().expect("joined"));
    });
    let r = heap.concurrent_ref_round_report();
    assert_eq!((r.rounds, r.round_us, r.max_round_us), (1, 1_234, 1_234));
    assert!(r.hold_waits >= 1, "the bounded wait, and the spawned one if it ran first");
    heap.note_reference_round_refusal(ZRefRoundRefusal::Vm);
    let r = heap.concurrent_ref_round_report();
    assert_eq!((r.refused, r.last_refusal), (1, ZRefRoundRefusal::Vm as u8));
}
