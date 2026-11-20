//! JIT round 21 wave 4, lane ic (`r21-receiver-profiling-gate-is-per-process`):
//! a store whose VM records no receivers stays silent while the process
//! gate is on for another VM, and the other store keeps recording.
use super::*;

#[test]
fn a_store_switched_off_records_nothing_while_another_records() {
    // Review rev-w68-jit: since wave 7 the master gate records receivers
    // on its own (`receivers_recording_answer`), so the `off` answers
    // below hold only while it is off; a parallel `with_profiling_enabled`
    // test would flip it. Held under the module's gate lock, off.
    tests::with_profiling_disabled(a_store_switched_off_records_nothing_body);
}

fn a_store_switched_off_records_nothing_body() {
    // Only ever switched ON here, so a concurrent test cannot be disturbed.
    enable_receiver_profiling(true);
    let on = ProfileStore::new();
    let off = ProfileStore::new();
    off.set_receivers_on(false);
    assert!(on.receivers_recording(), "a fresh store records as before");
    assert!(!off.receivers_recording());
    let key = MethodKey {
        class_id: 0x2141,
        method_name: Arc::from("r21IcPerVm"),
        descriptor: Arc::from("()V"),
    };
    assert!(on.seed_ic_observed_receiver(&key, 3, 42, 2));
    assert!(!off.seed_ic_observed_receiver(&key, 3, 42, 2));
    assert!(!off.claim_ic_seed_recompile(&key));
    off.set_receivers_on(true);
    assert!(off.seed_ic_observed_receiver(&key, 3, 42, 2), "its own answer, not the gate's");    }