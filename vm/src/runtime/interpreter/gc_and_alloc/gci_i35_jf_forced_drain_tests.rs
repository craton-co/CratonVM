use super::{forced_drain_left_to_delivery, DeliveryWait, FinalizerDelivery};

/// The decision matrix: only an unserved wait under the full hand-off,
/// with `leave_busy`, newly leaves the queue; a lock holder always does;
/// the default policy and the kill arm keep the inline drain.
#[test]
fn gci_i35jf_a_busy_delivery_thread_keeps_its_queue_under_the_full_hand_off() {
    use DeliveryWait::{BlockedOnCaller, Served, TimedOut};
    use FinalizerDelivery::{Always, Inline, WhenLockHeld};
    for policy in [Always, WhenLockHeld, Inline] {
        for leave in [true, false] {
            assert!(forced_drain_left_to_delivery(Served, false, policy, leave));
            assert!(forced_drain_left_to_delivery(TimedOut, true, policy, leave));
            assert!(forced_drain_left_to_delivery(
                BlockedOnCaller,
                true,
                policy,
                leave
            ));
        }
    }
    assert!(forced_drain_left_to_delivery(TimedOut, false, Always, true));
    assert!(!forced_drain_left_to_delivery(
        TimedOut, false, Always, false
    ));
    assert!(!forced_drain_left_to_delivery(
        TimedOut,
        false,
        WhenLockHeld,
        true
    ));
    assert!(!forced_drain_left_to_delivery(
        TimedOut, false, Inline, true
    ));
}

/// `runFinalization` never leaves the queue; the collection doors read
/// the switch.
#[test]
fn gci_i35jf_run_finalization_still_drains_and_collection_doors_read_the_switch() {
    let src = super::GC_AND_ALLOC_SRC;
    let body = |name: &str| -> String {
        let start = src.find(name).expect("the function");
        let rest = &src[start..];
        rest[..rest.find("\n\u{7d}\n").expect("its end")].to_string()
    };
    assert!(body("pub(crate) fn run_pending_finalizers_for_runtime(")
        .contains("run_finalizers_forced_as(shared, thread, false)"));
    assert!(body("fn run_finalizers_forced(")
        .contains("run_finalizers_forced_as(shared, thread, true)"));
    assert!(body("fn finalizers_handed_off_for_forced_drain(").contains(
        "forced_drain_left_to_delivery(wait_for_delivery(shared, thread, holds), holds, policy, leave_busy)"
    ));
}

/// JIT round 21 wave 23, lane finalize (the test item 2-4 review of
/// `docs/feature-designs/real-jdk-mode.md` asked
/// for; `CRATONVM_JNI_REINIT_REGISTERS_FINALIZER_AGAIN`): a counted JNI
/// re-`<init>` drives a SECOND `finalize()` through
/// `run_finalizers_impl`. The dead object's row is enqueued with one extra
/// registration and the object queued once; the drain takes the count,
/// queues the object again before the first run and dequeues it again in
/// the same pass. So the count is gone and the queue, which held the object
/// once, ends empty: two passes through the `finalize()` call. With the
/// switch off the count stays and the object runs once.
#[test]
fn r21w23_a_counted_reinit_runs_finalize_twice_through_the_door() {
    use crate::config::VmConfig;
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use crate::vm::SharedVm;
    for on in [true, false] {
        let shared = SharedVm::new(VmConfig {
            jni_reinit_registers_finalizer_again: on,
            ..VmConfig::default()
        });
        let class_id = shared
            .load_class_concurrent("java/lang/StringBuilder")
            .expect("load StringBuilder");
        let num_fields = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.num_total_fields)
            .expect("loaded");
        let obj = shared.mem.heap.alloc_object(class_id, num_fields);
        let addr = obj.as_ptr() as usize;
        shared.register_finalizable(addr);
        assert!(shared
            .mem
            .ref_processor
            .lock()
            .note_finalizer_registered_again(addr));
        // The object dies: its row is enqueued and the object queued once.
        shared.process_references(&|_| false, 100, 0);
        assert_eq!(shared.mem.finalizer_thread.pending_count(), 1);
        let mut thread = JvmThread::new(ThreadId(0x0F2D), "r21w23-finalize-twice");
        super::run_finalizers_impl(&shared, &mut thread, true);
        assert_eq!(
            shared.mem.finalizer_thread.pending_count(),
            0,
            "every run drained"
        );
        assert_eq!(
            shared
                .mem
                .ref_processor
                .lock()
                .extra_finalizers_outstanding(),
            if on { 0 } else { 1 },
            "the door took the extra run only with the switch on"
        );
        // Wave 24 (item 8 of `r21-review-w24-findings-FIXED-20261020.md`): the
        // re-queue was ACCEPTED, so the second run really went through the
        // queue (a refused re-queue also takes the count and leaves the
        // queue empty).
        assert_eq!(
            shared.mem.finalizer_thread.extra_runs_requeued(),
            if on { 1 } else { 0 },
            "the extra run was queued again"
        );
    }
}
