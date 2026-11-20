use super::*;
use crate::config::{GcAlgorithm, VmConfig};

/// An empty queue: the slice dequeues nothing and answers 0, so the
/// batch's loop ends (`ran == 0`).
#[test]
#[cfg(feature = "zgc")]
fn zt27fin1_an_empty_slice_answers_zero() {
    let shared = SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Zgc,
        ..VmConfig::default()
    });
    let mut thread = JvmThread::new(crate::ThreadId(4245), "zt27fin1-slice");
    assert_eq!(run_delivery_finalizer_slice(&shared, &mut thread), 0);
    // A zero bound dequeues nothing and answers 0 (worked by hand: the
    // bound check runs before the first dequeue).
    assert!(shared.mem.finalizer_thread.enqueue_unfinalized(0x4000));
    assert_eq!(run_finalizers_impl_upto(&shared, &mut thread, true, 0), 0);
    assert_eq!(shared.mem.finalizer_thread.pending_count(), 1);
}

/// Source pin: the batch loops over steps only under the switch, and the
/// slice is bounded by the queue length at its start and the deadline.
#[test]
fn zt27fin1_the_batch_loops_over_bounded_slices() {
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
    // Built at run time: an unbalanced brace inside a string literal
    // ends this module early for the production-panic scanners.
    let fn_end = format!("\n{}\n", char::from(0x7d_u8));
    let body = |sig: &str| {
        let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}`"));
        let end = start + src[start..].find(&fn_end).expect("its end");
        src[start..end].to_string()
    };
    let batch = body(concat!("fn drain_finalizer_", "thread_batch("));
    // zt t30/fz: the gate is `delivery_slices_finalizers` (ZGC's key, or
    // G1 / Generational's `CRATONVM_GC_FINALIZERS_OFF_CALLER_ALL`).
    let gate = batch
        .find("let sliced = delivery_slices_finalizers(shared)")
        .expect("the switch");
    let step = batch
        .find("run_delivery_finalizer_step(shared, thread, sliced)")
        .expect("the step");
    let exit = batch
        .find("if !sliced || ran == 0 || shared.mem.finalizer_thread.pending_count() == 0")
        .expect("the loop's end");
    assert!(gate < step && step < exit);
    let slice = body(concat!("fn run_delivery_finalizer_", "slice("));
    let limit = slice.find("pending_count()").expect("the bound");
    let one = slice
        .find("run_finalizers_impl_upto(shared, thread, false, 1)")
        .expect("one at a time");
    let deadline = slice
        .find("DELIVERY_FINALIZER_SLICE")
        .expect("the deadline");
    assert!(limit < one && deadline < one);
}
