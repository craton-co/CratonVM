use super::*;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

fn byte() -> u8 {
    JIT_REF_STORE_GATES.pre_active.load(Ordering::SeqCst)
}

#[test]
fn gci_i12wb_the_state_word_decodes_count_and_legacy_bit() {
    assert!(!pre_state_armed(0));
    assert!(pre_state_armed(1));
    assert!(pre_state_armed(PRE_LEGACY_BIT));
    assert!(!pre_state_armed(PRE_INFLIGHT_ONE * 3));
    assert!(pre_state_armed(PRE_INFLIGHT_ONE | 2));
    assert_eq!(PRE_COUNT_MASK & PRE_LEGACY_BIT, 0);
    assert_eq!(PRE_LEGACY_BIT & (PRE_INFLIGHT_ONE - 1), PRE_LEGACY_BIT);
}

#[test]
fn gci_i12wb_an_armed_marker_reads_armed_and_counts() {
    arm_jit_ref_store_marker();
    assert_eq!(byte(), 1);
    assert!(jit_ref_store_armed_markers() >= 1);
    disarm_jit_ref_store_marker();
}

/// The race of item 2: a peer arming and disarming in a loop (its disarm
/// refresh is the stale writer) must never clear the byte while this
/// thread holds a count.
#[test]
fn gci_i12wb_a_racing_disarm_never_clears_a_held_arm() {
    let stop = Arc::new(AtomicBool::new(false));
    let peer = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                arm_jit_ref_store_marker();
                disarm_jit_ref_store_marker();
            }
        })
    };
    let mut cleared = 0usize;
    for _ in 0..2_000 {
        arm_jit_ref_store_marker();
        for _ in 0..64 {
            if byte() == 0 {
                cleared += 1;
            }
        }
        disarm_jit_ref_store_marker();
    }
    stop.store(true, Ordering::Relaxed);
    peer.join().expect("peer thread");
    assert_eq!(cleared, 0, "the gate read disarmed while a marker held a count");
}
