//! gc-common round 2026-09-23, wave 6, lane C6: the allocation front end's
//! counters are per VM (`common-f-process-global-allocation-state`).

use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::memory::heap::ArrayElementType;
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

/// Two VMs in one process, one allocating thread on VM B:
/// * B's `thread_allocated_total` (the `getTotalThreadAllocatedBytes`
///   source) receives the thread's pre-first-refill external bytes (a
///   sub-batch, so held back until then) at the first refill's attach,
///   then its retired TLAB span, exactly once;
/// * B's refill-time trigger counters move;
/// * VM A's total and wedge counters do not move at all. With the old
///   process statics, A's answers included B's allocation.
#[test]
fn thread_allocation_total_and_wedge_counters_are_per_vm() {
    let vm_a = gen_vm();
    let vm_b = gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "w6c-per-vm");

    // A shared-path object before any refill: noted on the thread's
    // first buffer (`Tlab::empty()`), which the shared path attaches to
    // B's total; a sub-batch note is held back.
    let _obj = alloc_object_shared(&vm_b, &mut thread, ClassId::new(0), 3)
        .expect("an empty heap has room for one object");
    let external = thread.tlab.thread_allocated_bytes();
    assert!(external > 0);
    assert_eq!(vm_b.mem.thread_allocated_total.load(Ordering::Relaxed), 0);
    assert_eq!(
        thread.tlab.vm_thread_unpublished_bytes(),
        external,
        "the reader's own-thread term covers the pre-attach bytes"
    );

    // The first TLAB refill attaches this thread to B's total and credits
    // the pre-attach bytes once.
    let _arr = tlab_alloc_array_guarded_refill(
        &mut thread,
        &vm_b,
        ClassId::new(0),
        ArrayElementType::Int,
        4,
    )
    .expect("an empty young generation serves a TLAB refill");
    assert_eq!(
        vm_b.mem.thread_allocated_total.load(Ordering::Relaxed),
        external
    );
    assert!(
        vm_b.mem.tlab_wedge.refill_bytes_since_rearm() > 0,
        "the successful refill feeds B's healthy-path re-arm metric"
    );
    // zt t16/pl7: and B's never-reset refill total (`sun.gc.tlab.alloc`),
    // by the same bytes: one refill, neither reset yet.
    assert_eq!(
        vm_b.mem
            .tlab_wedge
            .refill_bytes_total
            .load(Ordering::Relaxed),
        vm_b.mem.tlab_wedge.refill_bytes_since_rearm()
    );

    // Retiring settles the live span into B's total, and only once.
    thread.tlab.retire();
    thread.tlab.retire();
    assert_eq!(
        vm_b.mem.thread_allocated_total.load(Ordering::Relaxed),
        thread.tlab.thread_allocated_bytes()
    );
    assert_eq!(thread.tlab.vm_thread_unpublished_bytes(), 0);

    // VM A saw nothing.
    assert_eq!(vm_a.mem.thread_allocated_total.load(Ordering::Relaxed), 0);
    let wedge = &vm_a.mem.tlab_wedge;
    assert_eq!(wedge.refill_bytes_since_rearm(), 0);
    assert_eq!(wedge.refill_bytes_total.load(Ordering::Relaxed), 0);
    assert_eq!(wedge.slowpath_entries_since_gc.load(Ordering::Relaxed), 0);
    assert_eq!(wedge.gate_consecutive_fails.load(Ordering::Relaxed), 0);
    assert_eq!(wedge.last_break_alloc_total.load(Ordering::Relaxed), 0);
}

/// Source witness for the retirement grep: none of the old process
/// statics is declared any more.
#[test]
fn the_wedge_counters_are_not_process_statics() {
    let src = concat!(include_str!("../gc_and_alloc/mod.rs"), "\n", include_str!("../gc_and_alloc/mtroots_on.rs"), "\n", include_str!("../gc_and_alloc/gc_notifications_go_to_delivery.rs"), "\n", include_str!("../gc_and_alloc/zgc_refusal_line.rs"), "\n", include_str!("../gc_and_alloc/finalizers_forced.rs"), "\n", include_str!("../gc_and_alloc/reference_round.rs"), "\n", include_str!("../gc_and_alloc/on_out_of_memory_error_commands.rs"), "\n", include_str!("../gc_and_alloc/frozen_peer_referents.rs"), "\n", include_str!("../gc_and_alloc/g1_force_full_cycle.rs"));
    for name in [
        "TLAB_GATE_CONSECUTIVE_FAILS",
        "TLAB_LAST_BREAK_ALLOC_TOTAL",
        "TLAB_SLOWPATH_ENTRIES_SINCE_GC",
        "TLAB_REFILL_BYTES_SINCE_GC",
    ] {
        let decl = format!("\nstatic {name}:");
        assert!(!src.contains(&decl), "{name} is a process static again");
    }
}
