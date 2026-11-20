//! gc-common round 2026-09-23, wave 4, lane A: the doors' hand-off of
//! reference work and GC notifications. The delivery thread itself needs a
//! VM built by `Vm::new` (see `w3d_finalizer_thread_tests`); these pin the
//! paths a unit fixture can reach, which are the flag-off / no-owner ones.

use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::vm::SharedVm;

fn fixture_vm() -> SharedVm {
    SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    })
}

/// `maybe_gc`'s no-collection path consumes the "reference work queued
/// elsewhere" hint exactly once, whether it drains inline (flag off) or
/// hands the work to a delivery thread (flag on; a fixture VM has no owner
/// to start one with, so it falls back to the inline drain). With nothing
/// queued, the drain finds nothing to run.
#[test]
fn the_no_collection_path_consumes_the_ref_work_hint() {
    let shared = fixture_vm();
    let mut thread = JvmThread::new(crate::ThreadId(1), "w4a-door");
    shared.mem.gc_barrier.note_ref_work_queued();
    assert!(shared.mem.gc_barrier.ref_work_queued());
    drain_ref_work_queued_elsewhere(&shared, &mut thread);
    assert!(
        !shared.mem.gc_barrier.ref_work_queued(),
        "the hint must be lowered by the drain that took it"
    );
    // A second call is the one-load fast path.
    drain_ref_work_queued_elsewhere(&shared, &mut thread);
    assert!(!shared.mem.gc_barrier.ref_work_queued());
}

/// With nothing queued the doors keep their own (instant) inline
/// notification call: no delivery thread is woken or started for an empty
/// queue, whatever the flag says.
#[test]
fn an_empty_notification_queue_starts_no_delivery_thread() {
    let shared = fixture_vm();
    let thread = JvmThread::new(crate::ThreadId(1), "w4a-door");
    assert!(!super::super::gc_events::gc_notifications_pending(&shared));
    assert!(!gc_notifications_go_to_delivery_thread(&shared, &thread));
    assert!(!shared.mem.finalizer_thread.delivery_thread_claimed());
}
