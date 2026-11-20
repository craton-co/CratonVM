use super::*;
use crate::config::{GcAlgorithm, VmConfig};

/// A bound of zero dequeues nothing (the bound is checked before the
/// dequeue), so a door bounded by the queue length at its start cannot
/// run what a peer queues after it.
#[test]
#[cfg(feature = "zgc")]
fn zt18x14_a_zero_bound_dequeues_nothing() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Zgc,
        ..VmConfig::default()
    });
    let mut thread = JvmThread::new(crate::ThreadId(4243), "zt18x14-bound");
    // Never dequeued: the addresses are never read.
    assert!(shared.mem.finalizer_thread.enqueue_unfinalized(0x1000));
    assert!(shared.mem.finalizer_thread.enqueue_unfinalized(0x2000));
    run_finalizers_impl_upto(&shared, &mut thread, true, 0);
    assert_eq!(shared.mem.finalizer_thread.pending_count(), 2);
}

/// Source pin: the collection door hands off or drains bounded and never
/// reaches either wait; `runFinalization` still takes the w5-d path.
#[test]
fn zt18x14_the_collection_door_waits_for_no_finalizer() {
    let src = concat!(
        include_str!("../gc_and_alloc/mod.rs"),
        "\n",
        include_str!("../gc_and_alloc/mtroots_on.rs"),
        "\n",
        include_str!("../gc_and_alloc/gc_notifications_go_to_delivery.rs"),
        "\n",
        include_str!("../gc_and_alloc/zgc_refusal_line.rs"),
        "\n",
        include_str!("../gc_and_alloc/finalizers_forced.rs"),
        "\n",
        include_str!("../gc_and_alloc/reference_round.rs"),
        "\n",
        include_str!("../gc_and_alloc/on_out_of_memory_error_commands.rs"),
        "\n",
        include_str!("../gc_and_alloc/frozen_peer_referents.rs"),
        "\n",
        include_str!("../gc_and_alloc/g1_force_full_cycle.rs")
    )
    .replace("\r\n", "\n");
    let end = format!("\n{}\n", char::from(0x7d_u8));
    let body = |sig: &str| {
        let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}`"));
        let stop = start + src[start..].find(end.as_str()).expect("its end");
        src[start..stop].to_string()
    };
    let door = body(concat!("fn run_finalizers_for_", "collection_door("));
    assert!(door.contains("run_finalizers_forced_inline(shared, thread, queued)"));
    assert!(!door.contains("wait_for_delivery"));
    assert!(!door.contains("wait_out_in_flight"));
    let forced = body(concat!("fn run_finalizers_", "forced("));
    let gate = forced
        .find("if system_gc_skips_finalizer_wait(shared)")
        .expect("the switch");
    let old = forced
        .find("run_finalizers_forced_as(shared, thread, true)")
        .expect("the kill arm");
    assert!(gate < old);
    let runtime = body(concat!(
        "pub(crate) fn run_pending_finalizers_",
        "for_runtime("
    ));
    assert!(!runtime.contains("system_gc_skips_finalizer_wait"));
    let upto = body(concat!("fn run_finalizers_impl_", "upto("));
    let check = upto.find("if ran >= limit").expect("the bound");
    let pop = upto
        .find("finalizer_thread.dequeue()")
        .expect("the dequeue");
    assert!(check < pop);
}
