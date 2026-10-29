// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane W6-B — a DECLINED `check_ihop` poll must be cheap to decline, and it
//! must give the same answer it gave the expensive way.
//!
//! # The defect these tests pin
//!
//! `CRATONVM_G1_IHOP_BACKOFF` suppresses a mark cycle. A suppressed cycle never
//! sets `g1_is_marking_active()`, and `g1_is_marking_active()` is the ONLY
//! thing that short-circuits the JIT mark driver every compiled `new` calls
//! (`jit_new_object`, `jit_newarray`, `jit_anewarray_object` →
//! `jit_drive_g1_concurrent_mark` → `g1_should_start_marking` → `check_ihop`).
//! So arming the back-off converts a gate consulted a few hundred times per run
//! into one consulted at the ALLOCATION rate: wave 3 measured `ihop_polls`
//! 18 → 1 616 077, wave 5 measured 348 → 41 641 213 against 130 mark cycles.
//!
//! Each of those polls performed two read-modify-writes on two process-global
//! cache lines — `IHOP_POLLS` and `mark_backoff_suppressions`. Both batteries
//! were single-threaded, so both measured those lines uncontended and both said
//! plainly that the N-thread cost was unmeasured.
//! `tools/probes/G1PollStormProbe.java` is the arm that measures it;
//! `CRATONVM_G1_IHOP_POLL_GATE` is the fix, and this file is the fix as
//! assertions.
//!
//! # What has to be true for the fix to be a fix rather than a shortcut
//!
//! The fast path answers from a published watermark instead of recomputing the
//! gate. That is only sound because every input to the gate
//! (`last_cycle_reclaimed_bytes`, `last_cycle_end_old_bytes`,
//! `mark_backoff_regions`) has exactly one writer — `note_mark_cycle_outcome` —
//! and that writer CLEARS the watermark. The tests below are that argument,
//! one assertion per link:
//!
//! * it declines exactly where the slow path declines
//!   (`the_fast_path_agrees_with_the_gate_it_replaces`);
//! * growth past the watermark still reopens the gate
//!   (`growth_past_the_watermark_still_opens_the_gate`);
//! * a completed cycle invalidates the watermark
//!   (`a_completed_cycle_clears_the_published_watermark`);
//! * it stands down rather than shadowing the deadline fail-safe
//!   (`the_fast_path_stands_down_when_the_deadline_is_armed`);
//! * and it is inert with the back-off off, which is what makes it usable as
//!   the NULL ARM of its own A/B (`the_poll_gate_is_inert_with_the_back_off_off`).
//!
//! Every test states BOTH arms of every flag it names: the off arm pins today's
//! behaviour so a default flip shows up as a failing assertion, and the on arm
//! pins the fix.
//!
//! # Flag scope
//!
//! `flags::with_thread_overrides`, not `with_process_overrides`, and never
//! `std::env::set_var` — the same reasoning as
//! `gc/tests/g1_w5c_mark_backoff_deadline.rs`: both flags are read only inside
//! `check_ihop`, on the calling thread.

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

/// How many 2 MiB spans `growth_past_the_watermark_still_opens_the_gate` may
/// allocate before it gives up and fails.
///
/// The gate demands `mark_backoff_regions` REGIONS of growth, doubling per
/// futile cycle from 1; the fixture runs two futile cycles, so it asks for 4
/// MiB and two spans clear it. Six is headroom without risking the fixture: the
/// `core_and_burst(6)` setup holds 12 spans = 24 MiB, so the worst case here is
/// 36 MiB of a 64 MiB heap. An array allocation this fixture cannot serve does
/// not fail, it ABORTS the process — see the comment in that test.
const SPAN_BUDGET: usize = 6;

/// A 1% IHOP, so the level test is satisfied by a couple of humongous spans and
/// every test here is about the BACK-OFF rather than about the threshold.
fn collector() -> Arc<G1Collector> {
    Arc::new(G1Collector::new(G1CollectorConfig {
        heap_size: HEAP,
        region_size: REGION,
        initial_heap_size: HEAP,
        ihop_percent: 1,
        ..Default::default()
    }))
}

/// One span of old-generation occupancy: a humongous array, which
/// `recompute_old_gen_bytes` counts immediately rather than after promotion.
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

/// `G1OldBurstProbe`'s shape reduced to objects: a `core` holder that keeps its
/// spans, and a `burst` holder whose spans are dropped in a single store.
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

/// `mark_backoff_suppressions`, i.e. `backoff_declined_polls`, after this
/// thread's fast-path residue has been published.
///
/// The residue is the one thing the fix trades away (see
/// `IHOP_POLL_FLUSH_BATCH`), so every assertion on the counter goes through
/// here rather than reading it raw — a test that asserted on an unflushed
/// counter would be asserting on the batch size.
fn declined_polls(gc: &G1Collector) -> u64 {
    gc.flush_gated_polls();
    let (_, _, _, _, _, suppressions) = gc.ihop_model_state();
    suppressions
}

/// `backoff_fast_declines`, after this thread's residue has been published.
///
/// # This function exists because the first version of this file did not have it
///
/// Every assertion below went through `declined_polls` for the flush and then
/// read `gc.backoff_fast_declines()` RAW, which reads zero until a thread has
/// accumulated [`IHOP_POLL_FLUSH_BATCH`] = 1024 declines. Sixty-three declines
/// is not 1024, so the counter read 0, and **the reading that produces is
/// exactly the reading that means "the fix never engaged"** — the one this
/// lane's own acceptance note tells a reader to check first.
///
/// So the failure looked like an inert optimisation and was an unflushed
/// counter. That is the same shape as `w3c-instrumentation-audit.md` §1, where
/// `backoff_declined_polls=0` was read as "the back-off never had to fire" when
/// it meant "the lever has no caller": a zero that two different causes can
/// produce is not evidence until the instrument is known to be live. The fix is
/// to make the flush impossible to forget rather than to remember it, so there
/// is one accessor per counter and both flush.
fn fast_declines(gc: &G1Collector) -> u64 {
    gc.flush_gated_polls();
    gc.backoff_fast_declines()
}

/// Put the collector in the state the fix is about: a wide back-off gate over a
/// heap whose old generation is above the threshold and is not growing.
///
/// Returns `(core, burst, roots)`. Two futile cycles widen the gate, then the
/// burst is dropped — which changes nothing the gate can see, and is exactly
/// the window `w5c-the-back-off-is-blind-to-old-objects-dying.md` is about.
fn wide_gate_over_a_flat_heap(gc: &G1Collector) -> (ObjectRef, ObjectRef, [ObjectRef; 2]) {
    let (core, burst) = core_and_burst(gc, 6);
    let roots = [core, burst];
    for _ in 0..2 {
        full_cycle(gc, &roots);
    }
    for i in 0..6 {
        gc.set_field(burst, i, Value::Object(None));
    }
    (core, burst, roots)
}

// ---------------------------------------------------------------------------
// 1. the null arm
// ---------------------------------------------------------------------------

/// **This is what licenses `IHOP_POLL_GATE=1` as the NULL ARM of the battery in
/// `w6b-the-declined-poll-is-the-cost.md`.**
///
/// Nothing declines with the back-off off — `check_ihop` returns on the level
/// test — so the fast path is unreachable and the flag cannot execute. A null
/// arm that is merely "expected to be inert" measures the experimenter's
/// expectations; this one is inert by construction and the assertion says so.
#[test]
fn the_poll_gate_is_inert_with_the_back_off_off() {
    flags::with_thread_overrides(
        &[
            ("CRATONVM_G1_IHOP_BACKOFF", None),
            ("CRATONVM_G1_IHOP_POLL_GATE", Some("1")),
        ],
        || {
            let gc = collector();
            let (_core, _burst, _roots) = wide_gate_over_a_flat_heap(&gc);

            let before = fast_declines(&gc);
            for _ in 0..64 {
                assert!(
                    gc.check_ihop(),
                    "with the back-off off `check_ihop` is a pure level test and \
                     the occupancy is over the threshold"
                );
            }
            assert_eq!(
                fast_declines(&gc),
                before,
                "the fast decline sits INSIDE the back-off's decline, so with the \
                 back-off off it cannot run — which is what makes this flag a \
                 provably inert null arm"
            );
        },
    );
}

// ---------------------------------------------------------------------------
// 2. the fast path is the gate, not an approximation of it
// ---------------------------------------------------------------------------

/// The same 64 polls, with the back-off on, decline 64 times either way — and
/// with the gate armed all but the first decline without touching a shared
/// line.
///
/// The first poll is the slow one by construction: nothing has published a
/// watermark yet. That is the whole shape of the fix (one expensive decline per
/// change of the gate's inputs, instead of one per allocation) and it is
/// asserted rather than described.
#[test]
fn the_fast_path_agrees_with_the_gate_it_replaces() {
    // --- off arm: today's behaviour, every decline the expensive way.
    let slow_declines = flags::with_thread_overrides(
        &[
            ("CRATONVM_G1_IHOP_BACKOFF", Some("1")),
            ("CRATONVM_G1_IHOP_POLL_GATE", None),
        ],
        || {
            let gc = collector();
            let (_core, _burst, _roots) = wide_gate_over_a_flat_heap(&gc);
            let before = declined_polls(&gc);
            let fast_before = fast_declines(&gc);
            for _ in 0..64 {
                assert!(!gc.check_ihop(), "the widened gate refuses every poll");
            }
            assert_eq!(
                fast_declines(&gc),
                fast_before,
                "with the flag off nothing may take the fast path"
            );
            declined_polls(&gc) - before
        },
    );

    // --- on arm: the same verdict, reached without the writes.
    //
    // NOT named `fast_declines`: that is the accessor above, and a `let` of the
    // same name shadows it for everything after this statement. The closure
    // below is part of this initialiser so it would still resolve, which is
    // exactly the kind of "works by accident" the next edit breaks.
    let gated_arm_declines = flags::with_thread_overrides(
        &[
            ("CRATONVM_G1_IHOP_BACKOFF", Some("1")),
            ("CRATONVM_G1_IHOP_POLL_GATE", Some("1")),
        ],
        || {
            let gc = collector();
            let (_core, _burst, _roots) = wide_gate_over_a_flat_heap(&gc);
            let before = declined_polls(&gc);
            let fast_before = fast_declines(&gc);
            for _ in 0..64 {
                assert!(
                    !gc.check_ihop(),
                    "the fast path must reach the SAME verdict as the gate it \
                     stands for, or it is a behaviour change wearing a \
                     performance fix's name"
                );
            }
            assert_eq!(
                fast_declines(&gc) - fast_before,
                63,
                "63 of the 64 polls decline without a shared write; the first \
                 takes the long path because nothing has published a watermark \
                 yet, and publishing it is what that path is for"
            );
            declined_polls(&gc) - before
        },
    );

    assert_eq!(
        slow_declines, gated_arm_declines,
        "the counter must not change meaning: 64 declined polls are 64 declined \
         polls on both arms, whatever they cost"
    );
    assert_eq!(slow_declines, 64, "and there were 64 of them");
}

// ---------------------------------------------------------------------------
// 3. the fast path cannot wedge the collector
// ---------------------------------------------------------------------------

/// Growth past the watermark reopens the gate, on the arm where the gate is
/// answered from a cached number.
///
/// This is the property that keeps the fix from being a way to never mark
/// again: the watermark is an occupancy, the fast path compares live occupancy
/// against it, and crossing it falls through to the real gate.
#[test]
fn growth_past_the_watermark_still_opens_the_gate() {
    flags::with_thread_overrides(
        &[
            ("CRATONVM_G1_IHOP_BACKOFF", Some("1")),
            ("CRATONVM_G1_IHOP_POLL_GATE", Some("1")),
        ],
        || {
            let gc = collector();
            let (core, _burst, _roots) = wide_gate_over_a_flat_heap(&gc);

            assert!(!gc.check_ihop(), "publishes the watermark");
            let fast_before = fast_declines(&gc);
            assert!(!gc.check_ihop(), "and answers from it");
            assert_eq!(
                fast_declines(&gc) - fast_before,
                1,
                "test setup: the second poll must be the CACHED one, or what \
                 follows is not a test of the cache"
            );

            // Grow the old generation past the widened gate, ONE SPAN AT A TIME.
            //
            // The first version of this test allocated sixteen spans in one go
            // — 32 MiB into a 64 MiB fixture already holding 24 — and the VM's
            // out-of-heap path for an array ABORTS rather than unwinding:
            // `FATAL: G1: out of heap space for array allocation`, exit
            // 0xc0000409. That does not fail the test, it kills the binary, and
            // the three tests after it in this file never ran. An aborting test
            // is worse than a failing one because it hides its neighbours.
            //
            // Incremental is also the better experiment. The width the gate
            // demands doubles per futile cycle and there is no public accessor
            // for it, so a fixed span count is a guess that either wastes heap
            // or under-shoots. Allocating until the gate opens MEASURES the
            // width instead, and bounds the heap by construction: the loop
            // reports how many spans were needed, and the bound is small enough
            // that the fixture cannot run out.
            let grown = gc.alloc_object(ClassId::new(82), SPAN_BUDGET);
            let mut spans_needed = 0usize;
            let mut opened = false;
            for i in 0..SPAN_BUDGET {
                let s = span(&gc);
                gc.set_field(grown, i, Value::Object(Some(s)));
                spans_needed = i + 1;
                if gc.check_ihop() {
                    opened = true;
                    break;
                }
            }
            let _keep = (core, grown);

            assert!(
                opened,
                "the cached watermark is an OCCUPANCY, not a verdict: growth past \
                 it must fall through to the gate, or the fast path would be a \
                 way to stop marking permanently. {} spans of {} bytes did not \
                 reopen it (spans_needed={}, old_gen_bytes={})",
                SPAN_BUDGET,
                REGION / 4 * 8,
                spans_needed,
                gc.old_gen_bytes()
            );
        },
    );
}

/// A completed cycle rewrites every input to the gate, so the watermark it was
/// derived from must not survive it.
///
/// Without the clear in `note_mark_cycle_outcome` this is the one way the fast
/// path could disagree with the gate: a cycle that reclaimed well sets
/// `last_cycle_reclaimed_bytes > 0`, which the slow path answers `true` to
/// immediately, while a stale watermark would keep answering `false`.
#[test]
fn a_completed_cycle_clears_the_published_watermark() {
    flags::with_thread_overrides(
        &[
            ("CRATONVM_G1_IHOP_BACKOFF", Some("1")),
            ("CRATONVM_G1_IHOP_POLL_GATE", Some("1")),
        ],
        || {
            let gc = collector();
            let (core, burst, roots) = wide_gate_over_a_flat_heap(&gc);

            assert!(!gc.check_ihop(), "publishes the watermark");
            let fast_before = fast_declines(&gc);
            assert!(!gc.check_ihop(), "and answers from it");
            assert_eq!(
                fast_declines(&gc) - fast_before,
                1,
                "the second poll took the fast path — the state this test is about"
            );

            // The burst is already dropped, so this cycle reclaims a large
            // fraction of the old generation. Run it the way the back-off can
            // never refuse: with the flag off, which is the control
            // `dropping_a_large_old_set_does_not_re_arm_the_back_off` uses.
            let occupancy_before = gc.old_gen_bytes();
            flags::with_thread_overrides(&[("CRATONVM_G1_IHOP_BACKOFF", None)], || {
                full_cycle(&gc, &roots);
            });
            assert!(
                gc.old_gen_bytes() < occupancy_before,
                "test setup: this cycle must actually reclaim, or it says nothing \
                 about a stale watermark ({} -> {})",
                occupancy_before,
                gc.old_gen_bytes()
            );

            let fast_after_cycle = fast_declines(&gc);
            let verdict = gc.check_ihop();
            assert_eq!(
                fast_declines(&gc),
                fast_after_cycle,
                "the poll after a cycle must take the LONG path: the cycle just \
                 rewrote every input the watermark was computed from"
            );
            assert!(
                verdict,
                "and the long path's verdict is `true`, because the cycle \
                 reclaimed — a stale watermark would have said `false` forever"
            );

            let _keep = (core, burst);
        },
    );
}

// ---------------------------------------------------------------------------
// 4. the fix does not shadow the fail-safe
// ---------------------------------------------------------------------------

/// With `CRATONVM_G1_MARK_BACKOFF_DEADLINE` armed, the fast path stands down.
///
/// The deadline is a second exit from the same decline, on a clock
/// (`collection_count`) the watermark does not encode. A fast path that
/// answered `false` from occupancy alone would silently disable it — and
/// "the optimisation turned off the fail-safe" is the exact shape of defect
/// this round keeps finding, so it is pinned rather than commented.
#[test]
fn the_fast_path_stands_down_when_the_deadline_is_armed() {
    flags::with_thread_overrides(
        &[
            ("CRATONVM_G1_IHOP_BACKOFF", Some("1")),
            ("CRATONVM_G1_IHOP_POLL_GATE", Some("1")),
            ("CRATONVM_G1_MARK_BACKOFF_DEADLINE", Some("1")),
        ],
        || {
            let gc = collector();
            let (core, burst, roots) = wide_gate_over_a_flat_heap(&gc);

            let fast_before = fast_declines(&gc);
            for _ in 0..8 {
                assert!(
                    !gc.check_ihop(),
                    "the deadline's clock has not advanced, so the gate still refuses"
                );
            }
            assert_eq!(
                fast_declines(&gc),
                fast_before,
                "not one of those polls may take the fast path while the deadline \
                 is armed: the deadline reads a clock the watermark does not encode"
            );

            // Advance the deadline's clock and check the fail-safe still fires.
            let mut live: Vec<ObjectRef> = roots.to_vec();
            for _ in 0..8 {
                gc.young_collection(&mut live, &NoopMonitors);
            }
            assert!(
                gc.check_ihop(),
                "and with the clock advanced the deadline releases a cycle, which \
                 is the behaviour the fast path must not have removed"
            );

            let _keep = (core, burst);
        },
    );
}
