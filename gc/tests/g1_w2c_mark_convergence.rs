// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane W2-C — the marker's completion signal must be a statement about the
//! CYCLE, not about whichever worker wrote it last.
//!
//! # The shape
//!
//! `ConcurrentMarkController::is_quiesced()` is what the VM's cycle driver
//! polls to decide that concurrent marking has converged and the final remark
//! may begin (`VmHeap::g1_final_remark_and_cleanup`). It read one
//! `AtomicBool` — `ConcurrentMarkState::quiesced` — shared by every marking
//! worker. Worker A reaches a fixed point and publishes `true`; worker B comes
//! back from a park a microsecond later, writes `false` on its way into a step
//! that will find nothing, and the coordinator is told "not yet" by a worker
//! with nothing to say about the cycle.
//!
//! The direction is safe: a spurious `false` costs a poll interval, never
//! correctness. That is exactly why it is a LATENCY change and why it moves on
//! a measurement — `CRATONVM_G1_MARK_CONVERGENCE_EPOCH=1` (opt-in) reads
//! "every worker parked at a fixed point, and no work published since the step
//! that found it" instead.
//!
//! # What this file asserts
//!
//! The property that has to hold on BOTH arms, because it is the one a wrong
//! answer here would break: **the signal must never report convergence over
//! gray work that has not been scanned.** A completion signal that fires early
//! hands an STW remark a heap whose closure is incomplete.
//!
//! There is deliberately no wall-clock bound on "the on arm is faster". Two
//! such bounds were already widened in this round; a timing assertion on a
//! loaded host teaches a reader to re-run rather than to read. The throughput
//! claim belongs in the orchestrator's probe run, and the flag exists so it can
//! be made there.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cratonvm_gc::collector::{GarbageCollector, StopTheWorldToken};
use cratonvm_gc::g1::G1CollectorConfig;
use cratonvm_gc::g1_concurrent::ConcurrentMarkController;
use cratonvm_gc::heap::ArrayElementType;
use cratonvm_gc::G1Collector;
use cratonvm_types::flags;
use cratonvm_types::{ClassId, Value};

#[inline]
fn stw() -> StopTheWorldToken {
    // SAFETY: the test thread drives the cycle's STW phases itself; the only
    // other threads are the marking workers, which are not mutators.
    unsafe { StopTheWorldToken::new() }
}

fn collector() -> Arc<G1Collector> {
    Arc::new(G1Collector::new(G1CollectorConfig {
        heap_size: 64 * 1024 * 1024,
        region_size: 1024 * 1024,
        ..Default::default()
    }))
}

/// A wide frontier under a single root: the gray set starts as ONE entry, so
/// whichever worker pops it owns the whole frontier and the peers have to steal
/// — which is the state in which "one worker is idle" and "the cycle has
/// converged" are most easily confused.
fn wide_graph(gc: &G1Collector, children: usize) -> (cratonvm_types::ObjectRef, usize) {
    let root = gc.alloc_array(ClassId::new(71), ArrayElementType::Reference, children);
    for i in 0..children {
        let child = gc.alloc_object(ClassId::new(72), 1);
        let leaf = gc.alloc_object(ClassId::new(73), 0);
        gc.set_field(child, 0, Value::Object(Some(leaf)));
        gc.set_array_element(root, i, Value::Object(Some(child)))
            .expect("array store");
    }
    // root + children + leaves.
    (root, 1 + children * 2)
}

/// Run a real concurrent cycle under `arm` and assert that convergence, when it
/// is reported, is true.
fn convergence_is_never_premature(arm: &str) {
    const CHILDREN: usize = 20_000;

    let gc = collector();
    let (root, expected_objects) = wide_graph(&gc, CHILDREN);

    gc.start_concurrent_mark(&stw());
    gc.remark(&stw(), &[root]);

    let controller = ConcurrentMarkController::spawn(Arc::clone(&gc));

    // Generous against a loaded host: this is a liveness deadline, not a
    // performance assertion. A signal that never fires is the failure worth
    // reporting; a signal that takes 300 ms instead of 30 is not.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut converged = false;
    while Instant::now() < deadline {
        if controller.is_quiesced() {
            converged = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        converged,
        "[{arm}] the completion signal never fired — the cycle driver polls this \
         to decide the final remark may begin, so a signal that never fires is a \
         cycle that never ends"
    );

    // How much had been scanned at the instant convergence was reported. This
    // is the number that carries the property across the join below.
    let scanned_at_convergence: usize = gc.dbg_mark_worker_scans_public().iter().sum();

    // LANE W5-C (2026-09-21) — the join moved ABOVE the fixed-point assertion,
    // and the property is now carried by the scan counter instead of by the
    // ordering. W5-B saw this test fail once and then pass 5/5 on the same
    // build, and the diagnosis is a race in the TEST, not in the protocol:
    //
    // `concurrent_mark_step` returns false on three conditions — gray set
    // non-empty, `mark_active != 0` (F-12's `ActiveMarker`: a peer has popped
    // an object whose children are not pushed yet), and a pending overflow
    // rescan. Only the first is what the old assertion's message claimed. On
    // the shared-bool arm `is_quiesced` publishes as soon as the FIRST worker
    // reaches a fixed point, while its five peers may still be inside their own
    // final step — and each is counted in `mark_active` for its duration. A
    // step issued from the test thread in that window returns false over a
    // closure that is complete, and the `WORKER_POLL_MS` fallback park reopens
    // the same window every 250 ms per worker for as long as the controller
    // lives.
    //
    // The production driver has neither problem: `g1_final_remark_and_cleanup`
    // calls `request_stop_and_join()` FIRST — so `mark_active` is provably 0 —
    // and then loops `while !concurrent_mark_step(usize::MAX) {}`, so a
    // transient false costs an iteration. The old ordering asserted something
    // production never asks for, in an order production never uses.
    //
    // Nothing is weakened. If the signal HAD fired over unscanned gray work,
    // the drain below would scan it, and `scanned` would exceed
    // `scanned_at_convergence` — which is asserted. That is a strictly stronger
    // statement than the old single-shot `true`, and it is race-free.
    controller.request_stop_and_join().expect("workers joined");

    assert!(
        gc.concurrent_mark_step(usize::MAX),
        "[{arm}] the gray set was not at a fixed point even after every marking \
         worker had been joined — an STW remark starting here would be handed \
         an incomplete closure"
    );

    let scanned: usize = gc.dbg_mark_worker_scans_public().iter().sum();
    assert_eq!(
        scanned,
        scanned_at_convergence,
        "[{arm}] `is_quiesced` reported convergence over gray work that had not \
         been scanned: draining after the join scanned {} more objects, so the \
         closure was incomplete at the moment the signal fired",
        scanned.saturating_sub(scanned_at_convergence)
    );
    assert!(
        scanned >= expected_objects,
        "[{arm}] convergence was reported after scanning {scanned} of \
         {expected_objects} objects"
    );

    // And the marking really did reach the far end of the graph.
    gc.cleanup(&stw());
}

/// Today's shared bool.
#[test]
fn the_shared_flag_never_reports_convergence_early() {
    flags::with_process_overrides(&[("CRATONVM_G1_MARK_CONVERGENCE_EPOCH", None)], || {
        convergence_is_never_premature("shared-bool");
    });
}

/// The cycle-scoped signal.
///
/// `with_process_overrides` rather than the thread-scoped form: the marking
/// workers are threads this test did not create, and `is_quiesced` is read on
/// the test thread while the counters it reads are written on theirs. This is
/// the only test in this binary, so the process-wide scope has nothing to
/// collide with.
#[test]
fn the_cycle_scoped_signal_never_reports_convergence_early() {
    flags::with_process_overrides(&[("CRATONVM_G1_MARK_CONVERGENCE_EPOCH", Some("1"))], || {
        convergence_is_never_premature("convergence-epoch");
    });
}
