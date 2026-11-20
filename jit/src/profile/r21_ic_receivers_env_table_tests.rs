//! JIT round 21 wave 7, lane ic (r21-review-w45-vm-findings item 3): the
//! env-combination table of receiver recording, before wave 4 and now.
//! Pure: [`receivers_recording_answer`] is fed what `SharedVm::new`
//! (`vm/src/vm/vm_init.rs`, "Receiver-type and call-site recording") would
//! store, so no process gate is touched.
use super::*;

/// What VM init stores: `receivers_wanted = tier_pgo_receivers() &&
/// consumer` (consumer = JIT on, or a profile save, or AOT training).
fn wanted(receivers_env: bool, consumer: bool) -> bool {
    receivers_env && consumer
}

/// Pre-wave-4 doors: `is_receiver_profiling_enabled()` = master OR the
/// process gate, which held the LAST initialised VM's `receivers_wanted`.
fn before(master: bool, last_wanted: bool) -> bool {
    master || last_wanted
}

/// Now: `own` is this VM, `other` an optional second VM initialised after
/// it; `per_vm` is `CRATONVM_JIT_PROFILE_RECEIVERS_PER_VM`.
fn now(master: bool, own: bool, other: Option<bool>, per_vm: bool) -> bool {
    if per_vm {
        // The process gate only goes ON; the store keeps the VM's answer.
        let gate = own || other.unwrap_or(false);
        receivers_recording_answer(master, gate, own)
    } else {
        // Kill arm: the process gate holds the last VM's answer, the store
        // stays at its fresh `true`.
        let gate = other.unwrap_or(own);
        receivers_recording_answer(master, gate, true)
    }
}

#[test]
fn single_vm_answer_is_unchanged_for_every_env_combination() {
    // master: CRATONVM_TIER_PGO / _ALWAYS, or an open C2 branch window.
    for master in [false, true] {
        for receivers_env in [false, true] {
            for consumer in [false, true] {
                for per_vm in [false, true] {
                    let own = wanted(receivers_env, consumer);
                    assert_eq!(
                        now(master, own, None, per_vm),
                        before(master, own),
                        "master={master} receivers_env={receivers_env} \
                         consumer={consumer} per_vm={per_vm}"
                    );
                }
            }
        }
    }
}

#[test]
fn master_gate_records_receivers_even_with_receivers_off() {
    // CRATONVM_TIER_PGO=1 CRATONVM_TIER_PGO_RECEIVERS=0: recorded before
    // wave 4, and again now (wave 4 alone answered false here).
    let own = wanted(false, true);
    assert!(!own);
    assert!(now(true, own, None, true));
    assert!(now(true, own, Some(true), true));
    // A no-consumer VM (--nojit) while the master gate is on.
    assert!(now(true, wanted(true, false), None, true));
    // Neither gate: nothing.
    assert!(!now(false, own, None, true));
}

#[test]
fn only_the_per_process_leak_changes_answer() {
    for master in [false, true] {
        for own in [false, true] {
            for other in [false, true] {
                let per_vm = now(master, own, Some(other), true);
                // Per VM: master, or this VM's own answer -- never another VM's.
                assert_eq!(per_vm, master || own, "master={master} own={own} other={other}");
                // Kill arm: the old per-process answer, the leak included.
                assert_eq!(
                    now(master, own, Some(other), false),
                    before(master, other),
                    "kill arm master={master} own={own} other={other}"
                );
            }
        }
    }
    // The leak itself: own off, the other VM on, no master gate.
    assert!(before(false, true));
    assert!(!now(false, false, Some(true), true));
}
