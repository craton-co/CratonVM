//! Round i1 wave 20, lane L4: `maybe_gc` asks the occupancy triggers after
//! a refill, after a shared-heap allocation, or once the byte countdown
//! runs out — and the `gc_requested` latch on every call.

use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use cratonvm_types::ClassId;
use std::sync::atomic::Ordering;

fn gen_vm() -> SharedVm {
    SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        ..VmConfig::default()
    })
}

/// A thread that never polled polls at its first allocation.
#[test]
fn a_fresh_thread_polls_the_occupancy_triggers() {
    let thread = JvmThread::new(ThreadId(0), "i20-l4-fresh");
    assert!(occupancy_poll_due(&thread));
}

/// A refill resets the countdown; a poll re-arms it; TLAB hits inside it
/// skip the occupancy triggers until this thread has allocated
/// `GC_OCCUPANCY_POLL_BYTES` more.
#[test]
fn tlab_hits_skip_the_occupancy_poll_until_the_countdown_runs_out() {
    if FULL_GC_POLL_AFTER_EVERY_NEW {
        return;
    }
    let shared = gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "i20-l4-countdown");
    arm_occupancy_poll_countdown(&mut thread);
    let refills_before = shared.mem.tlab_refill_count.load(Ordering::Relaxed);
    let _first = gc_alloc_object(&shared, &mut thread, ClassId::new(0), 2)
        .expect("an empty young generation serves the first object");
    assert_ne!(
        shared.mem.tlab_refill_count.load(Ordering::Relaxed),
        refills_before,
        "the first allocation of a thread refills its TLAB"
    );
    assert!(occupancy_poll_due(&thread), "a refill resets the countdown");
    maybe_gc(&shared, &mut thread);
    assert!(!occupancy_poll_due(&thread), "a poll re-arms the countdown");
    let armed_at = thread.gc_occupancy_poll_at;
    let refills_at_arm = shared.mem.tlab_refill_count.load(Ordering::Relaxed);
    let mut crossed = false;
    for _ in 0..10_000 {
        let _o = gc_alloc_object(&shared, &mut thread, ClassId::new(0), 2)
            .expect("young has room for a few hundred small objects");
        if shared.mem.tlab_refill_count.load(Ordering::Relaxed) != refills_at_arm {
            // A buffer smaller than the countdown: the refill is the poll.
            assert!(occupancy_poll_due(&thread), "a refill resets the countdown");
            return;
        }
        if thread.tlab.thread_allocated_bytes() >= armed_at {
            assert!(occupancy_poll_due(&thread), "the countdown ran out");
            crossed = true;
            break;
        }
        assert!(
            !occupancy_poll_due(&thread),
            "a TLAB hit inside the countdown skips the occupancy poll"
        );
    }
    assert!(
        crossed,
        "10k small objects allocate more than the countdown"
    );
}

/// An allocation outside the TLAB moves the occupancy and resets the
/// countdown.
#[test]
fn a_shared_heap_allocation_resets_the_countdown() {
    let shared = gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "i20-l4-shared");
    arm_occupancy_poll_countdown(&mut thread);
    assert!(FULL_GC_POLL_AFTER_EVERY_NEW || !occupancy_poll_due(&thread));
    let _o = alloc_object_shared(&shared, &mut thread, ClassId::new(0), 3)
        .expect("an empty heap has room for one object");
    assert!(occupancy_poll_due(&thread));
}

/// The `gc_requested` latch is not an occupancy trigger: an armed
/// countdown does not delay it.
#[test]
fn the_request_latch_is_taken_inside_the_countdown() {
    let shared = gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "i20-l4-latch");
    arm_occupancy_poll_countdown(&mut thread);
    let cycles_before = shared.mem.gc_cycle_count.load(Ordering::Relaxed);
    shared.mem.gc_requested.store(true, Ordering::Relaxed);
    maybe_gc(&shared, &mut thread);
    assert!(
        !shared.mem.gc_requested.load(Ordering::Relaxed),
        "the latch is consumed"
    );
    assert!(
        shared.mem.gc_cycle_count.load(Ordering::Relaxed) > cycles_before,
        "the requested collection ran"
    );
}

/// gcf f1/c: the JIT helpers' pickup of a latched Generational inline
/// start takes the latch exactly once and runs the driver, which re-asks
/// the trigger (an empty old generation: nothing opens).
#[test]
fn gcf_f1c_jit_pickup_takes_the_inline_start_latch_once() {
    let shared = gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "gcf-f1c-pickup");
    let state = &shared.mem.concurrent_gc_state;
    assert!(!gen_inline_concurrent_start_pending(&shared));
    gen_inline_concurrent_start_pub(&shared, &mut thread);
    assert_eq!(state.census().inline_start_taken, 0, "nothing latched");
    assert!(state.latch_inline_start_request());
    assert!(gen_inline_concurrent_start_pending(&shared));
    gen_inline_concurrent_start_pub(&shared, &mut thread);
    assert!(!gen_inline_concurrent_start_pending(&shared), "taken");
    assert_eq!(state.census().inline_start_taken, 1);
    assert_eq!(
        state.census().initial_marks,
        0,
        "the driver re-asked the trigger: an empty generation opens no cycle"
    );
    gen_inline_concurrent_start_pub(&shared, &mut thread);
    assert_eq!(state.census().inline_start_taken, 1, "taken once per latch");
}
