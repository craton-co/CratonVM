use super::*;
use crate::config::{GcAlgorithm, VmConfig};

/// Source pin: the step opens before the batch's `run_finalizers_impl`
/// and its guard is dropped before the GC-notification step; the wait
/// takes the step under the switch and the batch wait otherwise.
#[test]
fn zt15x12_the_batch_brackets_only_its_finalizer_step() {
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
    let body = |sig: &str| {
        let start = src.find(sig).unwrap_or_else(|| panic!("`{sig}`"));
        // Built at run time: an unbalanced brace inside a string literal
        // ends this module early for the production-panic scanners.
        let fn_end = format!("\n{}\n", char::from(0x7d_u8));
        let end = start + src[start..].find(&fn_end).expect("its end");
        src[start..end].to_string()
    };
    // zt t27/fin1: the step is its own function now
    // (`run_delivery_finalizer_step`), so its guard drops when it
    // returns, before the batch's notification step.
    let step =
        body("\npub(in crate::runtime::interpreter::gc_and_alloc) fn run_delivery_finalizer_step(");
    let open = step.find("signal.begin_finalizer_step();").expect("opened");
    let guard = step
        .find("let _step = FinalizerStep(signal);")
        .expect("guarded");
    let fin = step
        .find("run_finalizers_impl(shared, thread, false)")
        .expect("the finalizers");
    let slice = step
        .find("run_delivery_finalizer_slice(shared, thread)")
        .expect("the slice");
    assert!(open < guard && guard < fin && guard < slice);
    let batch = body(
        "\npub(in crate::runtime::interpreter::gc_and_alloc) fn drain_finalizer_thread_batch(",
    );
    let called = batch
        .find("run_delivery_finalizer_step(shared, thread, sliced)")
        .expect("the step");
    let notes = batch
        .find("run_gc_notifications_as_notification_thread(shared, thread)")
        .expect("the notifications");
    assert!(
        called < notes,
        "the step returns before the notification step"
    );
    let wait = body("\npub(in crate::runtime::interpreter::gc_and_alloc) fn wait_out_in_flight_finalizer_batch(");
    let step = wait
        .find("wait_for_delivery_step(shared, thread, holds, DeliveryStep::Finalizers)")
        .expect("the step wait");
    let gate = wait
        .find("system_gc_waits_finalizer_step_only(shared)")
        .expect("the switch");
    let whole = wait
        .find("wait_for_delivery(shared, thread, holds)")
        .expect("the batch wait");
    assert!(gate < step && step < whole);
}
