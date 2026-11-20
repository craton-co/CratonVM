use super::*;

/// `gch-h1ak-first-thread-start-initialises-java-util-logging-FIXED-20261012`:
/// under the old process-wide `AtomicBool`, VM A's first thread start
/// consumed VM B's arming. Now each VM's arming is its own, taken exactly
/// once, and a never-armed VM (or identity 0, the mocks') is never armed.
#[test]
fn gci_wr_each_vm_takes_only_its_own_prewarm_arming_once() {
    const VM_A: usize = 0x6C1_0A01;
    const VM_B: usize = 0x6C1_0A02;
    const VM_C: usize = 0x6C1_0A03;
    for vm in [VM_A, VM_B, VM_C] {
        LOGGING_PREWARM_ARMED.forget(vm);
    }
    arm_logging_prewarm_on_first_thread_start(VM_A);
    arm_logging_prewarm_on_first_thread_start(VM_B);
    arm_logging_prewarm_on_first_thread_start(0);
    assert!(take_logging_prewarm_arming(VM_A), "A's first start runs the pre-warm");
    assert!(!take_logging_prewarm_arming(VM_A), "once per VM");
    assert!(take_logging_prewarm_arming(VM_B), "A did not consume B's arming");
    assert!(!take_logging_prewarm_arming(VM_C), "C was never armed");
    assert!(!LOGGING_PREWARM_ARMED.has_row(VM_C), "and the ask left no row");
    assert!(!take_logging_prewarm_arming(0), "identity 0 is never armed");
    arm_logging_prewarm_on_first_thread_start(VM_A);
    forget_vm_system_singletons(VM_A);
    assert!(!take_logging_prewarm_arming(VM_A), "teardown drops the arming");
    forget_vm_system_singletons(VM_B);
}
