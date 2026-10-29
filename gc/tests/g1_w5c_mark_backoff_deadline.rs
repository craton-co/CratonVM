// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane W5-C — the mark-cycle back-off is blind to old objects DYING, and the
//! deadline is the bound on that blindness.
//!
//! # The gap, stated as the sequence that produces it
//!
//! `check_ihop`'s back-off (`CRATONVM_G1_IHOP_BACKOFF`) refuses a cycle unless
//! the old generation has GROWN since a cycle that reclaimed nothing, and the
//! growth it demands doubles per unproductive cycle. The premise is "nothing
//! has changed, so a second cycle can only reach the first one's verdict".
//!
//! Occupancy detects promotion. It detects nothing else. An old object dies
//! when a mutator overwrites the last reference to it, and that event moves no
//! bytes, frees no region and changes no number `recompute_old_gen_bytes`
//! publishes — so the gate reads "unchanged" about a heap in which half the
//! old generation has just become garbage. Discovering that is the concurrent
//! marker's whole job, which makes the refusal circular: the gate declines the
//! cycle for want of evidence only the cycle can produce.
//!
//! Wave 3 measured the back-off on `G1ChurnPauseProbe`, whose retained set is
//! FIXED, and recorded that the failure mode was structurally absent from the
//! battery (`w3c-the-six-w2c-flags-measured.md` §3, third ground for not
//! flipping the default). These tests are that missing case, reduced to the
//! policy objects so it is a deterministic assertion rather than a workload.
//!
//! # Why a deadline and not a better test
//!
//! There is no better test. The information is not in the heap's occupancy, so
//! `CRATONVM_G1_MARK_BACKOFF_DEADLINE` bounds the deferral instead: after
//! `MARK_BACKOFF_DEADLINE_COLLECTIONS` collections, let one cycle through
//! anyway. The anti-spin property survives, because the spin wave 2 described
//! is a cycle starting with ZERO collections in between — and
//! `the_deadline_does_not_restore_the_back_to_back_spin` is the test that says
//! so rather than the comment.
//!
//! Every test states BOTH arms of every flag it names: the off arm pins
//! today's behaviour so a default flip shows up as a failing assertion, and
//! the on arm pins the fix.
//!
//! # Flag scope
//!
//! `flags::with_thread_overrides`, not `with_process_overrides`, and never
//! `std::env::set_var`. Both flags read here are consulted in exactly one
//! place — `check_ihop`, on the thread that calls it — so a thread-scoped
//! override covers every read, and a process-scoped one would additionally
//! need a serialising mutex against the rest of the suite (see
//! `flags::override_process`: "concurrent installs are not supported"). This
//! matches `gc/tests/g1_w2c_ihop_model.rs`, which pins the same gate.

use std::sync::Arc;

use cratonvm_gc::collector::{GarbageCollector, MonitorCleanup, StopTheWorldToken};
use cratonvm_gc::g1::G1CollectorConfig;
use cratonvm_gc::G1Collector;
use cratonvm_types::flags;
use cratonvm_types::{ClassId, ObjectRef, Value};

struct NoopMonitors;
impl MonitorCleanup for NoopMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

/// Test-only STW witness (I-17): these tests drive the cycle by hand with no
/// mutators running, so the invariant the token stands for holds trivially.
#[inline]
fn stw() -> StopTheWorldToken {
    // SAFETY: single-threaded test driver; no mutator is executing.
    unsafe { StopTheWorldToken::new() }
}

const HEAP: usize = 64 * 1024 * 1024;
const REGION: usize = 1024 * 1024;

/// A 1% IHOP, so the level test is satisfied by a couple of humongous spans
/// and every test here is about the BACK-OFF rather than about the threshold.
fn collector() -> Arc<G1Collector> {
    Arc::new(G1Collector::new(G1CollectorConfig {
        heap_size: HEAP,
        region_size: REGION,
        initial_heap_size: HEAP,
        ihop_percent: 1,
        ..Default::default()
    }))
}

/// One span of old-generation occupancy.
///
/// A humongous array is the cheapest way for a test to put real bytes into the
/// old generation: `recompute_old_gen_bytes` counts `HumongousStart` alongside
/// `Old`, and the allocation publishes the occupancy immediately instead of
/// waiting for an object to age through promotion.
fn span(gc: &G1Collector) -> ObjectRef {
    gc.alloc_array(
        ClassId::new(91),
        cratonvm_gc::heap::ArrayElementType::Long,
        REGION / 4,
    )
}

/// Drive one complete cycle by hand, exactly as
/// `VmHeap::g1_final_remark_and_cleanup` does.
fn full_cycle(gc: &G1Collector, roots: &[ObjectRef]) {
    gc.start_concurrent_mark(&stw());
    gc.remark(&stw(), roots);
    while !gc.concurrent_mark_step(usize::MAX) {}
    gc.cleanup(&stw());
}

/// Run `n` young collections over `roots`, which is what moves
/// `collection_count` — the deadline's clock.
fn collections(gc: &G1Collector, roots: &[ObjectRef], n: usize) {
    let mut live: Vec<ObjectRef> = roots.to_vec();
    for _ in 0..n {
        gc.young_collection(&mut live, &NoopMonitors);
    }
}

/// The probe's shape, reduced to objects: a `core` holder that keeps its spans
/// for the life of the test, and a `burst` holder whose spans are dropped in a
/// single store. Returns `(core_holder, burst_holder)`.
///
/// Both holders are plain objects with `spans` reference fields, so dropping
/// the burst is `set_field(burst, i, Value::Object(None))` — a mutator
/// reference overwrite, which is precisely the event the growth gate cannot
/// observe.
fn core_and_burst(gc: &G1Collector, spans: usize) -> (ObjectRef, ObjectRef) {
    let core = gc.alloc_object(ClassId::new(80), spans);
    let burst = gc.alloc_object(ClassId::new(81), spans);
    for i in 0..spans {
        let s = span(gc);
        gc.set_field(core, i, Value::Object(Some(s)));
        let b = span(gc);
        gc.set_field(burst, i, Value::Object(Some(b)));
    }
    (core, burst)
}

// ---------------------------------------------------------------------------
// 1. the gap
// ---------------------------------------------------------------------------

/// **The falsification the wave-3 battery could not perform.**
///
/// A grow phase makes the back-off's gate wide (two futile cycles, so it now
/// demands several regions of growth). Then the burst is dropped: a large
/// fraction of the old generation becomes garbage with no change whatever to
/// occupancy. The next cycle is worth that whole fraction — it is the only
/// thing that can find it — and the back-off refuses it.
#[test]
fn dropping_a_large_old_set_does_not_re_arm_the_back_off() {
    flags::with_thread_overrides(&[("CRATONVM_G1_IHOP_BACKOFF", Some("1"))], || {
        let gc = collector();
        let (core, burst) = core_and_burst(&gc, 6);
        let roots = [core, burst];

        assert!(
            gc.old_gen_bytes() >= gc.marking_threshold_bytes(),
            "test setup: the spans must put occupancy over the threshold ({} vs {})",
            gc.old_gen_bytes(),
            gc.marking_threshold_bytes()
        );

        // --- grow half: two cycles over a wholly live graph. Both are
        // genuinely futile, and widening the gate on them is the back-off
        // working as designed.
        for _ in 0..2 {
            full_cycle(&gc, &roots);
        }
        let occupancy_after_grow = gc.old_gen_bytes();

        // --- the drop. Every burst span becomes unreachable, in six stores.
        for i in 0..6 {
            gc.set_field(burst, i, Value::Object(None));
        }

        // Occupancy is the point: it has not moved, because nothing has
        // reclaimed anything and nothing can until a cycle runs.
        assert_eq!(
            gc.old_gen_bytes(),
            occupancy_after_grow,
            "the whole premise: dropping old data changes no number the growth \
             gate tests, so the gate cannot tell this heap from the one the \
             last cycle already answered"
        );

        // And here is the refusal, with the flag's own falsification condition
        // met: this is a cycle the heap genuinely needs.
        assert!(
            !gc.check_ihop(),
            "the back-off refuses the one cycle in this sequence that would have \
             reclaimed anything — the gap this lane exists to bound"
        );

        // The control: the same sequence with the back-off off runs the cycle
        // and gets the burst back. Without this the test above would be
        // consistent with "there was nothing to reclaim".
        flags::with_thread_overrides(&[("CRATONVM_G1_IHOP_BACKOFF", None)], || {
            assert!(gc.check_ihop(), "today's pure level test lets it through");
            full_cycle(&gc, &roots);
            assert!(
                gc.old_gen_bytes() < occupancy_after_grow,
                "and the cycle the back-off refused reclaims real bytes: {} -> {}",
                occupancy_after_grow,
                gc.old_gen_bytes()
            );
        });
    });
}

// ---------------------------------------------------------------------------
// 2. the bound
// ---------------------------------------------------------------------------

/// The same sequence with the deadline armed: the gate still refuses while the
/// collector has barely run, and lets a cycle through once enough collections
/// have completed that the last cycle's findings are spent.
#[test]
fn the_deadline_releases_a_cycle_the_growth_gate_refused() {
    flags::with_thread_overrides(
        &[
            ("CRATONVM_G1_IHOP_BACKOFF", Some("1")),
            ("CRATONVM_G1_MARK_BACKOFF_DEADLINE", Some("1")),
        ],
        || {
            let gc = collector();
            let (core, burst) = core_and_burst(&gc, 6);
            let roots = [core, burst];

            for _ in 0..2 {
                full_cycle(&gc, &roots);
            }
            let occupancy_after_grow = gc.old_gen_bytes();
            for i in 0..6 {
                gc.set_field(burst, i, Value::Object(None));
            }

            // Immediately after the drop the deadline has not expired, so the
            // deadline is NOT a way of disabling the back-off: the refusal
            // still stands here, and it still gets counted.
            assert!(
                !gc.check_ihop(),
                "the deadline is a bound on the deferral, not a removal of it — \
                 nothing has happened yet"
            );
            assert_eq!(
                gc.mark_backoff_deadline_releases(),
                0,
                "and the release counter must say so"
            );

            // Now let the collector actually run. `MARK_BACKOFF_DEADLINE_COLLECTIONS`
            // is 8; the loop bound is deliberately larger so the test asserts
            // "the deadline expires" rather than pinning the constant's exact
            // value, which is a tuning number and not the property under test.
            collections(&gc, &[core, burst], 12);

            assert!(
                gc.check_ihop(),
                "once a cycle's findings are spent the next cycle is a different \
                 question, whether or not occupancy moved"
            );
            assert_eq!(
                gc.mark_backoff_deadline_releases(),
                1,
                "the fail-safe is counted, because a fail-safe nobody can see is \
                 indistinguishable from one that never fired"
            );

            // And the released cycle is worth something, which is the whole
            // claim: it gets the dropped burst back.
            full_cycle(&gc, &roots);
            assert!(
                gc.old_gen_bytes() < occupancy_after_grow,
                "the released cycle reclaims the dropped set: {} -> {}",
                occupancy_after_grow,
                gc.old_gen_bytes()
            );
        },
    );
}

/// **A release is a release, not a poll.**
///
/// `check_ihop` is consulted from the allocation slow path; wave 3 measured
/// `ihop_polls=1 616 077` against two mark cycles and had to rename the
/// sibling counter `backoff_declined_polls` because of it
/// (`w3c-the-six-w2c-flags-measured.md` §3). A deadline that answered "true"
/// to every poll once expired would leave the gate OPEN rather than bounded,
/// and would report a poll count under a name that reads as a cycle count.
///
/// The release advances the clock, so a thousand polls across an expired
/// deadline produce exactly one release and then go back to being refusals.
#[test]
fn an_expired_deadline_releases_once_not_once_per_poll() {
    flags::with_thread_overrides(
        &[
            ("CRATONVM_G1_IHOP_BACKOFF", Some("1")),
            ("CRATONVM_G1_MARK_BACKOFF_DEADLINE", Some("1")),
        ],
        || {
            let gc = collector();
            let (core, burst) = core_and_burst(&gc, 6);
            let roots = [core, burst];
            for _ in 0..2 {
                full_cycle(&gc, &roots);
            }
            for i in 0..6 {
                gc.set_field(burst, i, Value::Object(None));
            }
            collections(&gc, &[core, burst], 12);

            let mut allowed = 0usize;
            for _ in 0..1_000 {
                if gc.check_ihop() {
                    allowed += 1;
                }
            }
            assert_eq!(
                allowed, 1,
                "the deadline bounds the rate of cycles; it does not open the gate"
            );
            assert_eq!(
                gc.mark_backoff_deadline_releases(),
                1,
                "and the counter is releases, not polls — the distinction wave 3 \
                 had to rename a counter over"
            );
        },
    );
}

/// The deadline is inert unless the back-off is on.
///
/// Stated as a test because the flag is opt-in on top of another opt-in flag,
/// and a reader looking at a four-arm measurement has to know that the
/// `DEADLINE`-only arm is a null arm by construction.
#[test]
fn the_deadline_is_inert_with_the_back_off_off() {
    flags::with_thread_overrides(
        &[
            ("CRATONVM_G1_IHOP_BACKOFF", None),
            ("CRATONVM_G1_MARK_BACKOFF_DEADLINE", Some("1")),
        ],
        || {
            let gc = collector();
            let (core, burst) = core_and_burst(&gc, 6);
            let roots = [core, burst];
            for _ in 0..2 {
                full_cycle(&gc, &roots);
            }
            assert!(
                gc.check_ihop(),
                "with the back-off off `check_ihop` is a pure level test and the \
                 deadline has nothing to override"
            );
            assert_eq!(
                gc.mark_backoff_deadline_releases(),
                0,
                "a release can only happen on a poll the growth gate refused, and \
                 with the back-off off there are none"
            );
        },
    );
}

/// **The property the deadline must not break.**
///
/// Wave 2's spin is a cycle that starts immediately after the previous one
/// ended — zero collections in between, every marking worker burning beside the
/// application. The deadline measures in collections precisely so that case is
/// still refused, and this is the test that says so rather than the comment on
/// the constant.
#[test]
fn the_deadline_does_not_restore_the_back_to_back_spin() {
    flags::with_thread_overrides(
        &[
            ("CRATONVM_G1_IHOP_BACKOFF", Some("1")),
            ("CRATONVM_G1_MARK_BACKOFF_DEADLINE", Some("1")),
        ],
        || {
            let gc = collector();
            let (core, burst) = core_and_burst(&gc, 6);
            let roots = [core, burst];

            // A wholly live graph, cycled repeatedly with NO collections in
            // between: the exact shape wave 2 set out to stop.
            for _ in 0..6 {
                full_cycle(&gc, &roots);
                assert!(
                    !gc.check_ihop(),
                    "back-to-back against an unchanged heap must still be refused \
                     with the deadline armed — the deadline's clock is collections, \
                     and none have happened"
                );
            }
            assert_eq!(
                gc.mark_backoff_deadline_releases(),
                0,
                "no release: the spin is exactly the case the deadline's units \
                 were chosen to exclude"
            );
            let (_, _, _, _, _, suppressions) = gc.ihop_model_state();
            assert!(
                suppressions >= 6,
                "and every refusal is still counted ({suppressions})"
            );
        },
    );
}

/// A productive cycle re-arms the trigger with no deadline involved, so the
/// deadline cannot be what is making a reclaiming collector work.
#[test]
fn a_productive_cycle_still_re_arms_without_the_deadline() {
    flags::with_thread_overrides(
        &[
            ("CRATONVM_G1_IHOP_BACKOFF", Some("1")),
            ("CRATONVM_G1_MARK_BACKOFF_DEADLINE", Some("1")),
        ],
        || {
            let gc = collector();
            let (core, burst) = core_and_burst(&gc, 6);
            full_cycle(&gc, &[core, burst]);

            // What `cleanup` hands `note_mark_cycle_outcome` after a cycle that
            // halved the old generation.
            let occupancy = gc.old_gen_bytes().max(REGION * 8);
            gc.note_mark_cycle_outcome(occupancy, occupancy / 2);
            assert!(gc.check_ihop(), "the growth gate itself lets this through");
            assert_eq!(
                gc.mark_backoff_deadline_releases(),
                0,
                "and it does so without consulting the deadline — the fail-safe \
                 must not be load-bearing on the ordinary path"
            );
        },
    );
}

/// The deadline's clock survives a flag flip mid-run, because
/// `note_mark_cycle_outcome` records it on BOTH arms.
///
/// Without that, the first deadline after `CRATONVM_G1_MARK_BACKOFF_DEADLINE`
/// was armed would be measured from process start, and a long-running process
/// would take its first release immediately — which reads in a log exactly like
/// the fail-safe firing on evidence.
#[test]
fn the_deadline_clock_is_recorded_on_both_arms() {
    let gc = collector();
    let (core, burst) = core_and_burst(&gc, 6);
    let roots = [core, burst];

    // Everything below here runs with the deadline OFF...
    flags::with_thread_overrides(
        &[
            ("CRATONVM_G1_IHOP_BACKOFF", Some("1")),
            ("CRATONVM_G1_MARK_BACKOFF_DEADLINE", None),
        ],
        || {
            collections(&gc, &roots, 40);
            full_cycle(&gc, &roots);
            assert!(!gc.check_ihop(), "the growth gate refuses, as today");
        },
    );

    // ...and the flip must not find a clock that started at zero.
    flags::with_thread_overrides(
        &[
            ("CRATONVM_G1_IHOP_BACKOFF", Some("1")),
            ("CRATONVM_G1_MARK_BACKOFF_DEADLINE", Some("1")),
        ],
        || {
            assert!(
                !gc.check_ihop(),
                "the 40 collections above happened BEFORE the last cycle ended, so \
                 they are not deadline credit — a clock kept only on the on arm \
                 would release here"
            );
            assert_eq!(gc.mark_backoff_deadline_releases(), 0);
        },
    );
}
