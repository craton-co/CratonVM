use super::*;

/// Two VMs served by one thread (and a second thread) each read only the
/// calls made for them; switching back to a VM keeps adding to its count,
/// and the process census still sees every call.
#[test]
fn t1y3_direct_helper_tallies_land_in_the_calling_vm_only() {
    let vm_a = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let vm_b = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    assert_ne!(vm_a.vm_identity, vm_b.vm_identity);
    let process_before = jit_direct_helper_jdk_only_refusals_in_process();
    std::thread::scope(|s| {
        s.spawn(|| {
            JIT_COUNTERS.with(|b| {
                b.bump_direct_helper(&vm_a, true);
                b.bump_direct_helper(&vm_a, false);
                b.bump_direct_helper(&vm_b, true);
                b.bump_direct_helper(&vm_b, true);
                b.bump_direct_helper(&vm_a, true);
            });
        });
        s.spawn(|| {
            JIT_COUNTERS.with(|b| b.bump_direct_helper(&vm_b, false));
        });
    });
    assert_eq!(jit_direct_helper_jdk_only_refusals(&vm_a), 2);
    assert_eq!(jit_direct_helper_jdk_only_admissions(&vm_a), 1);
    assert_eq!(jit_direct_helper_jdk_only_refusals(&vm_b), 2);
    assert_eq!(jit_direct_helper_jdk_only_admissions(&vm_b), 1);
    // Other tests may add concurrently, so only `>=`.
    assert!(jit_direct_helper_jdk_only_refusals_in_process() >= process_before + 4);
}

/// t1 wave 7, lane V6: a thread that keeps alternating between two VMs
/// holds ONE tally per VM (it used to register a fresh one on every
/// switch, growing its list and each VM's sink without bound), and every
/// call still lands in its own VM's count.
#[test]
fn t1v6_alternating_vms_reuse_their_tally() {
    let vm_a = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let vm_b = crate::vm::SharedVm::new(crate::config::VmConfig::default());
    let held = std::thread::scope(|s| {
        s.spawn(|| {
            JIT_COUNTERS.with(|b| {
                for _ in 0..50 {
                    b.bump_direct_helper(&vm_a, true);
                    b.bump_direct_helper(&vm_b, false);
                }
                b.direct_helper_tallies.lock().len()
            })
        })
        .join()
        .unwrap()
    });
    assert_eq!(held, 2, "one tally per VM, not one per switch");
    assert_eq!(jit_direct_helper_jdk_only_refusals(&vm_a), 50);
    assert_eq!(jit_direct_helper_jdk_only_admissions(&vm_a), 0);
    assert_eq!(jit_direct_helper_jdk_only_refusals(&vm_b), 0);
    assert_eq!(jit_direct_helper_jdk_only_admissions(&vm_b), 50);
}
