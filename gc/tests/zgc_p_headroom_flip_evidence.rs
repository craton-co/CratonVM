// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane-P (wave 4): the four counters that decide
//! `CRATONVM_ZGC_HEADROOM_BYPASSES_REARM`, and the reason the criterion they
//! replace could not be run.
//!
//! # What was wrong with the old criterion
//!
//! `gap-e-headroom-trigger-below-rearm.md` lists five things that would earn
//! the flip. The fifth is the engagement gate — the one that says whether the
//! other four measured anything:
//!
//! > **`trigger_headroom` on `[GC] zgc-trigger:` must become non-zero** in the
//! > `=1` arm on a workload where it was zero. If it does not, the arm did not
//! > engage and the run says nothing.
//!
//! `trigger_headroom` cannot answer that. It is charged whenever
//! `headroom_low` is true **and** `needs_gc` returned true, which includes
//! every collection admitted through the ordinary `gc_rearm` floor — so it can
//! be identical and non-zero in both arms while the bypass never fires once.
//! `the_headroom_tally_cannot_tell_the_two_arms_apart` below is that statement
//! as a test rather than as an argument.
//!
//! What replaces it is `ZgcRealHeap::headroom_flip_evidence()`:
//! `(starved_intervals, starved_max_gap_bytes, bypass_fired, bypass_suppressed)`,
//! printed at shutdown as `[GC] zgc-headroom:`. `starved` is the one that
//! matters most, because it is readable in the **default** arm: it counts the
//! intervals in which the arena went short of contiguous space and the
//! live-bytes floor refused a collection anyway — i.e. the windows in which an
//! allocation failure is `std::process::abort()` rather than a cycle.
//!
//! Driven through `set_headroom_bypasses_rearm` rather than the environment
//! variable, because the flag reader latches in a `OnceLock` and a test that
//! set the variable would fix the answer for every later test in the binary.

#![cfg(feature = "zgc")]

use cratonvm_gc::collector::{GarbageCollector, MonitorCleanup, StopTheWorldToken};
use cratonvm_gc::zgc::ZgcRealHeap;
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef};

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

#[inline]
fn stw() -> StopTheWorldToken {
    // SAFETY: single-threaded integration test; no other mutator exists.
    unsafe { StopTheWorldToken::new() }
}

const MIB: usize = 1024 * 1024;

fn churn_to(heap: &ZgcRealHeap, bytes: usize) {
    while heap.allocated_bytes() < bytes {
        heap.alloc_array(ClassId::new(0), ArrayElementType::Byte, 32 * 1024);
    }
}

/// Drive a small heap into the state the flag is about: `headroom_low` raised
/// by a genuine `alloc_raw` refusal, with `allocated` (live bytes) still below
/// `gc_rearm`. Returns once it is there, or panics — a fixture that silently
/// fails to reach its own state is how an engagement counter becomes a vacuous
/// green, which is the whole subject of this file.
fn into_the_starved_state() -> ZgcRealHeap {
    // Small heap, so the headroom margin (`max(capacity/128, 8 MiB)`) is a
    // large fraction of it and the arena runs short quickly.
    let heap = ZgcRealHeap::with_capacity(32 * MIB);
    let mut roots: Vec<ObjectRef> = Vec::new();

    // One collection first, so `gc_rearm` is sized from a post-sweep live
    // figure rather than from the constructor's default.
    churn_to(&heap, 26 * MIB);
    let _ = heap.collect_garbage(&stw(), &mut roots, &NoMonitors);

    // Then fill the arena with garbage nothing roots: `allocated` tracks live
    // bytes and the bump cursor never rewinds, so the arena runs short while
    // `allocated` stays low. That divergence IS the defect.
    for _ in 0..4096 {
        heap.alloc_array(ClassId::new(0), ArrayElementType::Byte, 32 * 1024);
        if heap.headroom_trigger_state().0 {
            break;
        }
    }
    assert!(
        heap.headroom_trigger_state().0,
        "the fixture never raised `headroom_low`, so nothing below is about \
         anything"
    );
    heap
}

/// **`starved` is readable with the flag OFF**, which is what makes the flip
/// decidable without first performing the flip.
///
/// This is the counter the old criterion needed and did not have. A probe runs
/// the default binary, reads `[GC] zgc-headroom: starved_intervals=`, and
/// learns whether the workload can reach the shape at all — before spending a
/// suite run on an A/B that might be measuring noise.
#[test]
fn the_starved_counter_is_charged_in_the_default_arm() {
    let heap = into_the_starved_state();
    heap.set_headroom_bypasses_rearm(false);

    // Poll the way the native boundary does. Whether any individual poll
    // returns true depends on where `gc_rearm` landed; what is asserted is the
    // accounting, not the verdict.
    for _ in 0..64 {
        let _ = heap.needs_gc();
    }

    let (starved, gap, fired, suppressed) = heap.headroom_flip_evidence();
    if starved == 0 {
        // The floor admitted the collection on its own, so this heap is not in
        // the shape the flag is about. Assert the accounting is coherent and
        // stop: reporting a bypass from a state that never needed one would be
        // the defect.
        assert_eq!(
            fired, 0,
            "no starved interval, yet the bypass was credited with admitting a \
             collection"
        );
        return;
    }
    assert_eq!(
        starved, 1,
        "sixty-four polls of ONE pending interval must charge the starved \
         counter once -- a per-poll counter here would measure the native-call \
         rate, which is the exact defect `trigger_seen` was introduced to fix"
    );
    assert_eq!(
        fired, 0,
        "the bypass is OFF; it cannot have admitted anything"
    );
    assert_eq!(
        suppressed, 0,
        "`bypass_suppressed` means the bypass was available and declined. With \
         the flag off it is not available, and charging it would make a \
         default-arm run look like a suppressed feature"
    );
    assert!(
        gap > 0,
        "a starved interval means `allocated` was strictly below `gc_rearm`, \
         so the recorded gap must be non-zero"
    );
}

/// **`bypass_fired` separates the two arms where `trigger_headroom` cannot.**
///
/// Same heap, same state, one setter apart — an A/B rather than a rebuild.
#[test]
fn the_bypass_counter_is_charged_only_when_the_bypass_admits() {
    let heap = into_the_starved_state();

    heap.set_headroom_bypasses_rearm(false);
    for _ in 0..8 {
        let _ = heap.needs_gc();
    }
    let (starved_off, _, fired_off, _) = heap.headroom_flip_evidence();
    assert_eq!(fired_off, 0, "the OFF arm cannot fire the bypass");
    if starved_off == 0 {
        return; // the floor admits on its own here; see the test above
    }

    heap.set_headroom_bypasses_rearm(true);
    assert!(
        heap.needs_gc(),
        "with the bypass armed, a heap that cannot serve a margin-sized \
         request must ask for a collection whatever `gc_rearm` says"
    );
    let (_, _, fired_on, suppressed_on) = heap.headroom_flip_evidence();
    assert_eq!(
        fired_on, 1,
        "the bypass admitted a collection the floor had refused, and that is \
         the ONLY reading that proves the `=1` arm engaged"
    );
    assert_eq!(
        suppressed_on, 0,
        "the previous cycle reclaimed something, so the narrowing term did not \
         decline"
    );

    // Back off again: the counter is cumulative, not a state, so it must not
    // retreat -- and no NEW charge may appear from the OFF arm.
    heap.set_headroom_bypasses_rearm(false);
    for _ in 0..8 {
        let _ = heap.needs_gc();
    }
    assert_eq!(
        heap.headroom_flip_evidence().2,
        1,
        "turning the bypass off must neither erase the charge nor add one"
    );
}

/// **`trigger_headroom` reads the same in both arms**, which is why criterion 5
/// of `gap-e-headroom-trigger-below-rearm.md` could not have decided anything.
///
/// Not a regression test for a bug — a standing record of why the criterion
/// changed. If someone re-derives the old criterion from the tally, this fails
/// the moment they try to use it as a discriminator.
#[test]
fn the_headroom_tally_cannot_tell_the_two_arms_apart() {
    let heap = into_the_starved_state();
    heap.set_headroom_bypasses_rearm(false);
    for _ in 0..8 {
        let _ = heap.needs_gc();
    }
    let tally_off = heap.trigger_tallies().2;

    heap.set_headroom_bypasses_rearm(true);
    for _ in 0..8 {
        let _ = heap.needs_gc();
    }
    let tally_on = heap.trigger_tallies().2;

    assert_eq!(
        tally_off, tally_on,
        "`trigger_headroom` is charged at most once per pending collection, so \
         it cannot distinguish an interval the floor admitted from one the \
         bypass admitted. Read `headroom_flip_evidence()` instead"
    );
}

/// **The remembered-set scan's baseless-page counter is zero on a real heap**,
/// which is the wave-3 prediction nobody had confirmed.
///
/// `young_extra_roots` resolves each card's page base through
/// `ZRememberedSetTable::iterate_slot_addresses`, which refuses a page with no
/// base rather than guessing. Every registration site on this heap supplies
/// one, so the refusal count must be zero — and a non-zero reading is a new
/// registrar using `register_old_page` instead of
/// `register_old_page_with_base_in`, not a heap state. Each count is a dropped
/// old→young edge, i.e. a live object freed.
#[test]
fn the_young_cycle_never_skips_a_page_for_want_of_a_base() {
    let heap = ZgcRealHeap::with_capacity(64 * MIB);
    heap.set_generational_enabled(true);
    let mut roots: Vec<ObjectRef> = Vec::new();

    for _ in 0..4 {
        churn_to(&heap, 50 * MIB);
        let _ = heap.collect_garbage(&stw(), &mut roots, &NoMonitors);
        assert_eq!(
            heap.remset_pages_without_base(),
            0,
            "the remembered-set scan skipped a registered old page with no base \
             address; every old->young edge on it was dropped this cycle"
        );
    }
}
