// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Three properties of the ZGC mark/barrier pair that are only visible when the
//! pieces are driven end to end, and one of which was a latent use-after-free
//! until 2026-09-20.
//!
//! # 1. A weak load must not heal a slot into the cycle's mark colour
//!
//! [`cratonvm_gc::zgc::barrier::z_weak_load`] deliberately does **not** call
//! `ZBarrierContext::mark_live` — that is what makes `WeakReference` and
//! `Cleaner` able to clear at all. Until this round it nevertheless *healed* the
//! slot, stamping `heal_color()` into it, and during concurrent marking
//! `heal_color()` is `Z_COLORED_TAG | good_mask()` where the good mask **is** the
//! mark colour.
//!
//! The consequence is not local. The next ordinary `getfield` of that same slot
//! classifies `Good` on the fast path, returns the address, and never reaches
//! the slow path where `mark_live` lives. So a referent the application has read
//! through a strong reference finishes the cycle unmarked and is swept: a
//! use-after-free whose crash site is arbitrarily far from the barrier.
//!
//! `barrier.rs` has its own unit test for the exit; what this file adds is the
//! **sequence** — weak load, then strong load, then ask whether the referent was
//! kept alive. A test that stopped at "the slot was not healed" would pass
//! against any heal colour that happened to be harmless on the day.
//!
//! # 2. `ZMarkContext::claim_child`'s default must be the old two calls exactly
//!
//! `claim_child` fused `is_in_heap` + `try_mark` into one virtual call on the
//! per-edge path. It is a defaulted trait method, so every existing implementor
//! silently moved onto it — which is only safe if the default is the identical
//! decision, **including the short-circuit**: an implementor whose `try_mark`
//! dereferences must never be asked about an address `is_in_heap` would have
//! refused. The context below records the call order and asserts it.
//!
//! # 3. `mark_to_completion` must not report a stopped pool as a success
//!
//! Its report used to carry only `budget_exhausted`, and the `should_stop`
//! escape at the head of its loop left that `false` — indistinguishable from a
//! converged mark to the one caller that decides whether the sweep may run.

// `cratonvm_gc::zgc` is `#[cfg(feature = "zgc")]`, and that feature is
// default-on. A `--no-default-features` build has no ZGC at all, so this whole
// file compiles away rather than failing to resolve the module.
#![cfg(feature = "zgc")]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cratonvm_gc::zgc::barrier::{
    z_load, z_weak_load, ZBarrierContext, ZBarrierStats, Z_COLORED_TAG, Z_MARKED0, Z_REMAPPED,
};
use cratonvm_gc::zgc::mark::{
    TestMarkContext, ZChildClaim, ZMarkContext, ZMarkCoordinator, ZMarkEndResult,
};
use rustc_hash::FxHashMap;

// ---------------------------------------------------------------------------
// 1. The weak-then-strong sequence
// ---------------------------------------------------------------------------

/// A barrier context in the **marking** phase: good colour is `Marked0`, the
/// heal colour carries the tag the way `vaddr` builds every colored word, and
/// nothing is relocating.
///
/// Everything it is asked is recorded, because the property under test is about
/// what the barrier *did not* do and an absence has to be observed rather than
/// assumed.
struct MarkingPhaseContext {
    marked: Mutex<Vec<u64>>,
    stats: ZBarrierStats,
    marking: AtomicBool,
}

impl MarkingPhaseContext {
    fn new() -> Self {
        MarkingPhaseContext {
            marked: Mutex::new(Vec::new()),
            stats: ZBarrierStats::new(),
            marking: AtomicBool::new(true),
        }
    }

    fn marked_addrs(&self) -> Vec<u64> {
        self.marked.lock().expect("mark log").clone()
    }
}

impl ZBarrierContext for MarkingPhaseContext {
    fn good_mask(&self) -> u64 {
        Z_MARKED0
    }
    fn is_marking(&self) -> bool {
        self.marking.load(Ordering::Relaxed)
    }
    fn is_relocating(&self) -> bool {
        false
    }
    fn forward(&self, addr: u64) -> Option<u64> {
        Some(addr)
    }
    fn mark_live(&self, addr: u64) {
        self.marked.lock().expect("mark log").push(addr);
    }
    fn stats(&self) -> &ZBarrierStats {
        &self.stats
    }
}

/// THE REGRESSION. Read a referent weakly, then read it strongly; the strong
/// read must still keep it alive.
///
/// The weak read is the reference processor deciding the referent's fate. The
/// strong read is ordinary application code — `Reference.get()` having handed
/// the object out, or simply another field pointing at the same object. Nothing
/// about the first read may make the second one stop marking.
#[test]
fn a_weak_read_does_not_make_a_later_strong_read_skip_marking() {
    let ctx = MarkingPhaseContext::new();
    // A slot holding a REMAPPED-coloured reference: bad during marking, so
    // both loads below start on the slow path unless something healed it.
    let referent: u64 = 0x4_2000;
    let slot = AtomicU64::new(referent | Z_REMAPPED | Z_COLORED_TAG);

    assert_eq!(z_weak_load(&slot, &ctx), referent);
    assert!(
        ctx.marked_addrs().is_empty(),
        "a weak load that marks its referent is a WeakReference that never clears"
    );

    // The strong load. This is the assertion the whole file exists for.
    assert_eq!(z_load(&slot, &ctx), referent);
    assert_eq!(
        ctx.marked_addrs(),
        vec![referent],
        "the strong load MUST have kept the referent alive. If this is empty, the \
         weak load healed the slot into the cycle's mark colour and the strong \
         load took the fast path straight past mark_live -- the referent then \
         finishes the cycle unmarked and the sweep frees an object the \
         application is holding"
    );
}

/// The same sequence with marking **off** — the phase in which healing a weak
/// load is both safe and worth having.
///
/// Pins the scope of the fix. Widening it to "weak loads never heal" would cost
/// real throughput during relocation, where a heal is what stops every load
/// re-walking the forwarding table, and there is no marker for a mark colour to
/// hide a referent from.
#[test]
fn a_weak_read_outside_marking_still_heals_the_slot() {
    let ctx = MarkingPhaseContext::new();
    ctx.marking.store(false, Ordering::Relaxed);
    let referent: u64 = 0x4_3000;
    let slot = AtomicU64::new(referent | Z_REMAPPED | Z_COLORED_TAG);

    assert_eq!(z_weak_load(&slot, &ctx), referent);
    assert!(ctx.marked_addrs().is_empty(), "weak never marks");
    assert_eq!(
        slot.load(Ordering::Relaxed),
        referent | Z_MARKED0 | Z_COLORED_TAG,
        "with no cycle running the good colour is not a mark colour, so the heal \
         cannot hide anything"
    );
    assert_eq!(ctx.stats().heal_cas_wins.load(Ordering::Relaxed), 1);
}

/// The accounting identity on `ZBarrierStats` must survive the new exit.
///
/// It is the property that says "no slow-path entrant vanished without
/// recording what happened to it", and a new early return is exactly the change
/// that breaks it silently.
#[test]
fn the_slow_path_accounting_identity_holds_across_a_weak_and_a_strong_load() {
    let ctx = MarkingPhaseContext::new();
    let weak_slot = AtomicU64::new(0x4_4000 | Z_REMAPPED | Z_COLORED_TAG);
    let strong_slot = AtomicU64::new(0x4_5000 | Z_REMAPPED | Z_COLORED_TAG);

    let _ = z_weak_load(&weak_slot, &ctx); // exit 5: no CAS
    let _ = z_load(&strong_slot, &ctx); // exit 4: one CAS

    let s = ctx.stats().snapshot();
    assert_eq!(s.slow_path_entries, 2);
    assert_eq!(
        s.heal_cas_wins + s.heal_cas_losses + s.heal_skipped,
        s.slow_path_entries,
        "every entrant must land in exactly one of wins/losses/skipped: {s:?}"
    );
    assert_eq!(s.heal_skipped, 1, "the weak load");
    assert_eq!(s.heal_cas_wins, 1, "the strong load");
    assert_eq!(s.weak_slow_paths, 1);
    assert_eq!(s.marks_enqueued, 1);
}

// ---------------------------------------------------------------------------
// 2. `claim_child`'s default
// ---------------------------------------------------------------------------

/// What a [`ZMarkContext`] was asked, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asked {
    InHeap(u64),
    TryMark(u64),
}

/// A context that records the questions rather than answering interestingly.
///
/// `in_heap` is a fixed set, so "was `try_mark` asked about an address outside
/// it" is decidable from the log alone.
struct RecordingContext {
    in_heap: Vec<u64>,
    log: Mutex<Vec<Asked>>,
    marked: Mutex<Vec<u64>>,
}

impl ZMarkContext for RecordingContext {
    fn good_mask(&self) -> u64 {
        Z_REMAPPED
    }
    fn try_mark(&self, addr: u64) -> bool {
        self.log.lock().expect("log").push(Asked::TryMark(addr));
        let mut marked = self.marked.lock().expect("marked");
        if marked.contains(&addr) {
            false
        } else {
            marked.push(addr);
            true
        }
    }
    fn is_marked(&self, addr: u64) -> bool {
        self.marked.lock().expect("marked").contains(&addr)
    }
    fn visit_refs(&self, _addr: u64, _f: &mut dyn FnMut(u64)) {}
    fn is_in_heap(&self, addr: u64) -> bool {
        self.log.lock().expect("log").push(Asked::InHeap(addr));
        self.in_heap.contains(&addr)
    }
}

/// The default `claim_child` is `is_in_heap` then `try_mark`, **in that order**,
/// and it short-circuits.
///
/// The order is not a style preference. `try_mark` on the production context
/// dereferences the address to reach its object header, so asking it about an
/// address `is_in_heap` would have refused is a wild-pointer dereference —
/// which is the entire reason the engine has a gate in front of the claim. A
/// defaulted method that got this backwards would have moved every existing
/// implementor onto the unsafe order without a single call site changing.
#[test]
fn claim_child_gates_before_it_claims_and_short_circuits() {
    let ctx = RecordingContext {
        in_heap: vec![10, 11],
        log: Mutex::new(Vec::new()),
        marked: Mutex::new(Vec::new()),
    };

    assert_eq!(ctx.claim_child(10), ZChildClaim::Claimed);
    assert_eq!(ctx.claim_child(10), ZChildClaim::AlreadyMarked);
    assert_eq!(ctx.claim_child(99), ZChildClaim::OffHeap);

    let log = ctx.log.lock().expect("log").clone();
    assert_eq!(
        log,
        vec![
            Asked::InHeap(10),
            Asked::TryMark(10),
            Asked::InHeap(10),
            Asked::TryMark(10),
            Asked::InHeap(99),
        ],
        "the refused address must never reach try_mark"
    );
}

/// The three outcomes stay distinguishable, because two of them are counted
/// differently by the engine and the third is the common case.
///
/// Collapsing `OffHeap` into `AlreadyMarked` would make a wild-pointer storm —
/// which is how a mutator racing a reference store, or a mis-wired barrier
/// domain, presents — read as ordinary marking.
#[test]
fn the_three_claim_outcomes_are_distinct() {
    assert_ne!(ZChildClaim::OffHeap, ZChildClaim::AlreadyMarked);
    assert_ne!(ZChildClaim::AlreadyMarked, ZChildClaim::Claimed);
    assert_ne!(ZChildClaim::OffHeap, ZChildClaim::Claimed);
}

// ---------------------------------------------------------------------------
// 3. The engine end to end, over batches on both sides of the stripe count
// ---------------------------------------------------------------------------

fn graph(edges: &[(u64, &[u64])]) -> FxHashMap<u64, Vec<u64>> {
    let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
    for (node, children) in edges {
        g.insert(*node, children.to_vec());
        for c in children.iter() {
            g.entry(*c).or_default();
        }
    }
    g
}

/// `ZMarkShared::publish_distributed` now spreads a batch as contiguous chunks
/// over `min(k, stripe_count)` stripes rather than dealing it round-robin over
/// all of them. The mark SET must be identical either way, at batch sizes both
/// below and above the stripe count — the change decides which worker reaches an
/// object first, never whether it is reached.
///
/// A batch of one is the interesting small case: `groups == 1`, so exactly one
/// stripe is touched and `chunks(1)` must not produce more chunks than there are
/// stripes.
#[test]
fn a_root_batch_smaller_than_the_stripe_count_is_still_fully_marked() {
    for roots in [1usize, 3, 17, 512] {
        let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
        let mut expected: Vec<u64> = Vec::new();
        for i in 0..roots as u64 {
            let root = 1000 + i;
            let child = 900_000 + i;
            g.insert(root, vec![child]);
            g.insert(child, Vec::new());
            expected.push(root);
            expected.push(child);
        }
        expected.sort_unstable();

        let ctx = Arc::new(TestMarkContext::new(g));
        let pool = ZMarkCoordinator::new(ctx.clone(), 4);
        assert!(
            pool.stripe_count() >= 16,
            "this test wants batches on both sides of the stripe count"
        );
        pool.begin_cycle();
        let root_addrs: Vec<u64> = (0..roots as u64).map(|i| 1000 + i).collect();
        assert_eq!(pool.push_roots(&root_addrs), roots);
        let report = pool.mark_to_completion(64);
        pool.end_cycle();

        assert!(report.mark_set_complete(), "roots={roots}: {report:?}");
        assert_eq!(ctx.marked_sorted(), expected, "roots={roots}");
        assert_eq!(report.stats.objects_scanned, expected.len() as u64);
        pool.shutdown();
    }
}

/// A converged run reports `mark_set_complete()`, and the accessor is the one
/// question a caller deciding whether to sweep should ask.
///
/// The field it replaces — `budget_exhausted` alone — was the field
/// `ZgcRealHeap::finish_concurrent_mark` branches on, and it stayed `false` when
/// the loop left because the POOL was stopped. That report certified a partial
/// mark set. This asserts the healthy shape so the unhealthy one has something
/// to differ from.
#[test]
fn a_converged_run_reports_a_complete_mark_set() {
    let g = graph(&[(1, &[2, 3]), (2, &[4]), (3, &[4, 5]), (4, &[]), (5, &[])]);
    let ctx = Arc::new(TestMarkContext::new(g));
    let pool = ZMarkCoordinator::new(ctx.clone(), 2);

    pool.begin_cycle();
    assert_eq!(pool.push_roots(&[1]), 1);
    let report = pool.mark_to_completion(64);
    pool.end_cycle();

    assert!(report.mark_set_complete(), "{report:?}");
    assert!(!report.budget_exhausted);
    assert!(!report.stopped_early);
    assert_eq!(ctx.marked_sorted(), vec![1, 2, 3, 4, 5]);
    assert_eq!(pool.try_end_mark(), ZMarkEndResult::Complete);
    pool.shutdown();
}

/// A pool reused across cycles must report each cycle's own counters.
///
/// `ZMarkStats::reset` said "zero every counter" and left three behind, two of
/// which are per-cycle: `mark_end_restarts` is the number the mark-end warning
/// calls "marking is losing the race with the application", and reading a
/// process-cumulative value there makes that judgement about the whole run while
/// printing it against one collection.
#[test]
fn a_reused_pool_reports_per_cycle_restart_and_yield_counts() {
    let g = graph(&[(1, &[2]), (2, &[3]), (3, &[])]);
    let ctx: Arc<dyn ZMarkContext> = Arc::new(TestMarkContext::new(g));
    let pool = ZMarkCoordinator::new(Arc::new(cratonvm_gc::zgc::mark::ZInertMarkContext), 2);

    for round in 0..3 {
        pool.begin_cycle_with(Arc::clone(&ctx));
        pool.push_roots(&[1]);
        let report = pool.mark_to_completion(64);
        pool.end_cycle();
        assert!(report.mark_set_complete(), "round {round}: {report:?}");
        assert_eq!(
            report.stats.mark_end_restarts, 0,
            "round {round}: a quiet cycle restarts zero times, and a cumulative \
             counter would carry a previous cycle's restarts into this one"
        );
        assert_eq!(
            report.stats.yields, 0,
            "round {round}: nothing paused these workers"
        );
        // Only the FIRST cycle marks anything: the context's mark bits persist,
        // so rounds 2 and 3 find everything already claimed. That is the
        // property that makes `roots_marked` a per-cycle number worth resetting.
        let expected_roots = u64::from(round == 0);
        assert_eq!(report.stats.roots_marked, expected_roots, "round {round}");
    }
    pool.shutdown();
}
