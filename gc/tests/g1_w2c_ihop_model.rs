// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane W2-C — what the adaptive IHOP model measures, and what stops it
//! marking forever.
//!
//! Two defects, one file, because they are the two halves of the same
//! question: *when should a concurrent cycle start?*
//!
//! # 1. The model measured the wrong growth
//!
//! F-15's adaptive IHOP predicts "how much room must be left when a cycle
//! starts so that marking finishes before the heap fills" as
//! `alloc_rate × mark_ms`. `note_mark_cycle_end` derived `alloc_rate` from
//! `old_gen_bytes` read at the very END of `cleanup` — i.e. AFTER
//! `recompute_old_gen_bytes` has applied that cycle's in-place Old frees and
//! its dead-humongous reclaim. That is the occupancy the cycle LEFT BEHIND,
//! not the occupancy it had to survive, and freeing at the end of a cycle does
//! not retroactively give the marker headroom it did not have.
//!
//! A steady-state cycle — one that reclaims roughly what it promoted, which is
//! the shape the whole feature was built for — therefore folds `grew == 0`,
//! `recompute_marking_threshold` takes its "nothing measured" branch, and the
//! adaptive controller silently disengages behind the static IHOP. A second,
//! smaller error sat beside it: `(grew / 1024) as u64 / elapsed_ms` truncates
//! twice, so any cycle growing under 1 KiB/ms (1 MB/s, an ordinary rate) also
//! read as exactly zero.
//!
//! # 2. Nothing stopped it marking back-to-back forever
//!
//! `check_ihop` was `old_bytes >= threshold` and nothing else. A LIVE old set
//! above the threshold marks continuously: the cycle reclaims nothing,
//! `cleanup` republishes the same occupancy, and the level test fires again —
//! burning every marking worker beside the application, precisely when the
//! heap is tightest.
//!
//! Both fixes are opt-in (`CRATONVM_G1_IHOP_GROSS_GROWTH`,
//! `CRATONVM_G1_IHOP_BACKOFF`), so each test states BOTH arms: the off arm
//! pins today's behaviour so a default flip is a visible diff, and the on arm
//! pins the fix.

use std::sync::Arc;

use cratonvm_gc::collector::{GarbageCollector, StopTheWorldToken};
use cratonvm_gc::g1::G1CollectorConfig;
use cratonvm_gc::G1Collector;
use cratonvm_types::flags;
use cratonvm_types::{ClassId, Value};

/// Test-only STW witness (I-17): this file drives the cycle by hand with no
/// mutators running, so the invariant the token stands for holds trivially.
#[inline]
fn stw() -> StopTheWorldToken {
    // SAFETY: single-threaded test driver; no mutator is executing.
    unsafe { StopTheWorldToken::new() }
}

const HEAP: usize = 64 * 1024 * 1024;
const REGION: usize = 1024 * 1024;

fn collector(ihop_percent: u8) -> Arc<G1Collector> {
    Arc::new(G1Collector::new(G1CollectorConfig {
        heap_size: HEAP,
        region_size: REGION,
        ihop_percent,
        ..Default::default()
    }))
}

// ---------------------------------------------------------------------------
// 1. the measurement
// ---------------------------------------------------------------------------

/// The double truncation, stated as the number it produces.
///
/// 512 bytes per millisecond is 0.5 KiB/ms, so `(grew / 1024) / elapsed` floors
/// to 0 — and a zero rate is what sends `recompute_marking_threshold` down its
/// "nothing measured" branch and leaves the static IHOP in force. The model is
/// not wrong here; it is ABSENT, which is worse, because a threshold that never
/// moves reads exactly like one the model chose.
#[test]
fn a_sub_kib_per_ms_growth_rate_is_no_longer_floored_to_zero() {
    // 200 ms during which the old generation grew by 100 KiB: 512 B/ms.
    const ELAPSED_MS: u64 = 200;
    const GREW: usize = 100 * 1024;

    // OFF arm — today. The rate reads zero and the threshold stays at the
    // operator's ceiling, i.e. the measurement changed nothing.
    flags::with_thread_overrides(&[("CRATONVM_G1_IHOP_GROSS_GROWTH", None)], || {
        let gc = collector(70);
        let ceiling = gc.marking_threshold_bytes();
        gc.fold_mark_cycle_sample(ELAPSED_MS, GREW);
        assert_eq!(
            gc.marking_threshold_bytes(),
            ceiling,
            "today's double truncation reports 0 KiB/ms for a 512 B/ms cycle, so \
             the threshold cannot move"
        );
        let (_, rate, _, _, disengaged, _) = gc.ihop_model_state();
        assert_eq!(rate, 0, "the truncated rate");
        assert_eq!(
            disengaged, 1,
            "and the disengagement must be COUNTED on the default build — that \
             counter is the half of this fix that is not behind a flag"
        );
    });

    // ON arm — the rate is derived in bytes and the threshold reserves the
    // headroom the measurement asks for.
    flags::with_thread_overrides(&[("CRATONVM_G1_IHOP_GROSS_GROWTH", Some("1"))], || {
        let gc = collector(70);
        let ceiling = gc.marking_threshold_bytes();
        gc.fold_mark_cycle_sample(ELAPSED_MS, GREW);
        assert!(
            gc.marking_threshold_bytes() < ceiling,
            "a measured 512 B/ms growth must reserve headroom (ceiling {ceiling}, \
             threshold {})",
            gc.marking_threshold_bytes()
        );
        let (_, _, _, _, disengaged, _) = gc.ihop_model_state();
        assert_eq!(
            disengaged, 0,
            "a cycle that measured something must not be counted as disengaged"
        );
    });
}

/// The threshold is compared against `old_gen_bytes`, which counts Old plus
/// Humongous and nothing else — so `heap_size - headroom` hands the old
/// generation the young generation's footprint as if it were free space.
///
/// The operator's `ihop_percent` ceiling usually swallows the difference, which
/// is why this test drives a case where it cannot: a headroom small enough that
/// `heap - headroom` still sits ABOVE the ceiling, so the off arm is pinned at
/// the ceiling and only the young subtraction can move it.
#[test]
fn the_threshold_leaves_room_for_the_young_generation() {
    // An exact multiple of 1 KiB/ms, so the rate is identical on both arms and
    // the young subtraction is the only difference between them.
    const ELAPSED_MS: u64 = 1;
    const GREW: usize = 4 * 1024 * 1024; // 4 MiB/ms → 4 MiB of headroom

    let baseline = flags::with_thread_overrides(&[("CRATONVM_G1_IHOP_GROSS_GROWTH", None)], || {
        let gc = collector(90);
        let ceiling = gc.marking_threshold_bytes();
        gc.fold_mark_cycle_sample(ELAPSED_MS, GREW);
        let after = gc.marking_threshold_bytes();
        assert_eq!(
            after, ceiling,
            "test setup: with a 90% IHOP on a {HEAP}-byte heap, `heap - 4 MiB` \
                 is still above the ceiling, so today's planner is pinned there and \
                 any movement below comes from the young subtraction alone"
        );
        after
    });

    flags::with_thread_overrides(&[("CRATONVM_G1_IHOP_GROSS_GROWTH", Some("1"))], || {
        let gc = collector(90);
        let young_bytes = gc.young_target_regions() * REGION;
        assert!(
            young_bytes > 0,
            "test setup: the adaptive young target must be non-zero for this to \
             mean anything"
        );
        gc.fold_mark_cycle_sample(ELAPSED_MS, GREW);
        let after = gc.marking_threshold_bytes();
        assert!(
            after < baseline,
            "the young generation is not available to the old one: threshold must \
             drop below the ceiling ({after} vs {baseline}, young {young_bytes})"
        );
    });
}

/// A cycle that folds nothing is a controller that has stopped controlling, and
/// nothing said so. It does now, on BOTH arms — an operator on the default
/// build has to be able to see the disengaged state in order to decide whether
/// to flip the flag.
#[test]
fn a_zero_growth_cycle_is_counted_as_a_disengagement_on_the_default_build() {
    flags::with_thread_overrides(&[("CRATONVM_G1_IHOP_GROSS_GROWTH", None)], || {
        let gc = collector(70);
        let ceiling = gc.marking_threshold_bytes();
        for _ in 0..3 {
            gc.fold_mark_cycle_sample(50, 0);
        }
        assert_eq!(
            gc.marking_threshold_bytes(),
            ceiling,
            "the static IHOP is what is actually in force"
        );
        let (_, _, _, _, disengaged, _) = gc.ihop_model_state();
        assert_eq!(disengaged, 3, "and every one of those cycles is named");
    });
}

// ---------------------------------------------------------------------------
// 2. the back-off
// ---------------------------------------------------------------------------

/// Occupancy that IHOP can see, built out of a humongous object.
///
/// A humongous span is the cheapest way for an integration test to put real
/// bytes into the old generation: `recompute_old_gen_bytes` counts
/// `HumongousStart` alongside `Old`, and the allocation publishes the occupancy
/// immediately rather than waiting for a promotion. Everything here is
/// reachable from `root`, so a cycle over it reclaims nothing — which is
/// exactly the shape the back-off exists for.
fn humongous_root(gc: &G1Collector) -> cratonvm_types::ObjectRef {
    gc.alloc_array(
        ClassId::new(91),
        cratonvm_gc::heap::ArrayElementType::Long,
        REGION / 4,
    )
}

/// Drive one complete cycle by hand, exactly as
/// `VmHeap::g1_final_remark_and_cleanup` does.
fn full_cycle(gc: &G1Collector, roots: &[cratonvm_types::ObjectRef]) {
    gc.start_concurrent_mark(&stw());
    gc.remark(&stw(), roots);
    while !gc.concurrent_mark_step(usize::MAX) {}
    gc.cleanup(&stw());
}

#[test]
fn a_cycle_that_reclaimed_nothing_does_not_immediately_re_fire() {
    // A 1% IHOP so the level test is satisfied by a couple of humongous spans
    // and the test is about the back-off rather than about the threshold.
    let gc = collector(1);
    let root = humongous_root(&gc);
    let roots = [root];

    assert!(
        gc.old_gen_bytes() >= gc.marking_threshold_bytes(),
        "test setup: the humongous span must put occupancy over the threshold \
         ({} vs {})",
        gc.old_gen_bytes(),
        gc.marking_threshold_bytes()
    );

    // Everything is live, so this cycle gets nothing back.
    let before = gc.old_gen_bytes();
    full_cycle(&gc, &roots);
    assert_eq!(
        gc.old_gen_bytes(),
        before,
        "test setup: the graph is wholly live, so the cycle must reclaim nothing"
    );

    // OFF arm — today: the level test is still true, so the VM starts another
    // cycle against the identical heap, which can only reach the identical
    // verdict. That is the spin.
    flags::with_thread_overrides(&[("CRATONVM_G1_IHOP_BACKOFF", None)], || {
        assert!(
            gc.check_ihop(),
            "today's pure level test re-fires against an unchanged heap"
        );
    });

    // ON arm — declined, and counted.
    flags::with_thread_overrides(&[("CRATONVM_G1_IHOP_BACKOFF", Some("1"))], || {
        assert!(
            !gc.check_ihop(),
            "a cycle that reclaimed nothing must not be repeated until the old \
             generation has grown: the next cycle would be given the same heap"
        );
        let (_, _, _, _, _, suppressions) = gc.ihop_model_state();
        assert!(
            suppressions >= 1,
            "and a collector that declines to collect must be visible ({suppressions})"
        );
    });

    // Growth re-arms it. This is what stops the back-off from being a ban: any
    // change to the heap makes the next cycle a different question.
    let grown = humongous_root(&gc);
    assert!(
        gc.old_gen_bytes() > before,
        "test setup: the second span must raise occupancy ({} vs {before})",
        gc.old_gen_bytes()
    );
    flags::with_thread_overrides(&[("CRATONVM_G1_IHOP_BACKOFF", Some("1"))], || {
        assert!(
            gc.check_ihop(),
            "one region's growth re-arms the trigger — the back-off suppresses \
             only 'the same heap again', never reclamation"
        );
    });
    // Keep the second span reachable for the life of the test so nothing above
    // depends on it having been collected.
    let keep = gc.alloc_object(ClassId::new(92), 1);
    gc.set_field(keep, 0, Value::Object(Some(grown)));
}

/// The complementary property: a cycle that DID reclaim well must re-arm the
/// trigger immediately. Written separately because the gate above could have
/// been implemented as "never fire twice", which would pass that test and stop
/// the collector.
#[test]
fn a_productive_cycle_re_arms_the_trigger_at_once() {
    flags::with_thread_overrides(&[("CRATONVM_G1_IHOP_BACKOFF", Some("1"))], || {
        let gc = collector(1);
        let root = humongous_root(&gc);
        full_cycle(&gc, &[root]);

        // Record a cycle that got a large share of the old generation back.
        // `note_mark_cycle_outcome` is the same entry point `cleanup` uses; the
        // numbers here are what a reclaiming cycle would hand it.
        let occupancy = gc.old_gen_bytes().max(REGION * 8);
        gc.note_mark_cycle_outcome(occupancy, occupancy / 2);
        assert!(
            gc.check_ihop(),
            "a cycle that halved the old generation is evidence that another one \
             is worth running, not evidence against it"
        );
    });
}
