// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `ZMarkShared::publish_distributed`'s chunking, which wave 1 changed without
//! compiling and named as its riskiest unverified edit.
//!
//! # What changed, and what could have gone wrong
//!
//! The function used to deal a batch of `k` addresses out one at a time
//! (`i % n_stripes`) into `n_stripes` freshly-allocated vectors. It now walks
//! `addrs.chunks(per_group).enumerate()` and hands chunk `i` to stripe `i`.
//! That is on the **mutator** path — `ZgcRealHeap::hand_satb_batch_to_the_marker`
//! routes every SATB batch through `ZMarkCoordinator::push_roots`, which lands
//! here — so it is worth having, and the arithmetic is worth checking:
//!
//! ```text
//!   groups    = min(n_stripes, k)
//!   per_group = ceil(k / groups)
//!   chunks    = ceil(k / per_group)          <- what `chunks()` actually yields
//! ```
//!
//! The failure this file exists to exclude is `chunks > n_stripes`, which would
//! make `enumerate()` produce a stripe index past the end. It does not panic —
//! `ZMarkStripeSet::publish_slice` masks the key — so the symptom would be two
//! chunks silently landing in one stripe and a worker starting empty, i.e. the
//! exact load imbalance the spread exists to prevent, visible only as
//! throughput. `ceil(k / ceil(k / g)) <= g` holds for all positive integers, but
//! "holds for all positive integers" is what the round-robin version's author
//! believed too.
//!
//! # Why an integration test rather than a unit test
//!
//! `publish_distributed` is private. Its public entry is
//! [`ZMarkCoordinator::push_roots`], and the observable is the stripe set, so
//! the test drives the real path: build a pool, push roots, and look at where
//! they landed **before** `start_marking` lets a worker take any of them.

use std::sync::{Arc, Mutex};

use cratonvm_gc::zgc::mark::{
    stripe_count_for, ZMarkContext, ZMarkCoordinator, Z_MARK_MAX_STRIPES, Z_MARK_MIN_STRIPES,
};

/// The smallest context that satisfies the trait: every nonzero address is in
/// heap, every address is markable once, nothing has children.
///
/// Deliberately not `TestMarkContext`: that one builds an adjacency map and
/// this test never traces an edge. What is under test is where `push_roots`
/// *puts* the claimed addresses, not what marking does with them afterwards.
#[derive(Default)]
struct FlatContext {
    marked: Mutex<std::collections::HashSet<u64>>,
}

impl ZMarkContext for FlatContext {
    fn good_mask(&self) -> u64 {
        // `Z_REMAPPED`. The engine does not branch on this; it is surfaced for
        // logging. Spelled as a literal rather than imported so this file has
        // no opinion about the colour encoding.
        1 << 44
    }
    fn try_mark(&self, addr: u64) -> bool {
        self.marked.lock().expect("poisoned").insert(addr)
    }
    fn is_marked(&self, addr: u64) -> bool {
        self.marked.lock().expect("poisoned").contains(&addr)
    }
    fn visit_refs(&self, _addr: u64, _f: &mut dyn FnMut(u64)) {}
    fn is_in_heap(&self, addr: u64) -> bool {
        addr != 0
    }
}

/// The arithmetic `publish_distributed` performs, restated here so the test
/// asserts against a *derivation* rather than against a table of magic numbers
/// copied out of the implementation.
fn expected_chunks(k: usize, n_stripes: usize) -> usize {
    let groups = n_stripes.min(k);
    let per_group = k.div_ceil(groups);
    k.div_ceil(per_group)
}

/// `(stripe_count, stripes_touched, total_entries)` after pushing `k` distinct
/// roots into a pool of `workers` workers.
///
/// `start_marking` is deliberately never called: the workers stay parked in
/// `wait_for_cycle`, so nothing can steal from a stripe between the push and
/// the measurement.
fn push_and_measure(k: usize, workers: usize) -> (usize, usize, usize, Vec<usize>) {
    let ctx: Arc<FlatContext> = Arc::new(FlatContext::default());
    let pool = ZMarkCoordinator::new(ctx, workers);
    pool.begin_cycle();

    // Ids from 1: address 0 is the engine's null everywhere.
    let roots: Vec<u64> = (1..=k as u64).collect();
    let claimed = pool.push_roots(&roots);
    assert_eq!(claimed, k, "every distinct root must be newly claimed");

    let stripes = pool.shared().stripes();
    let n = stripes.stripe_count();
    let mut occupied = Vec::new();
    for i in 0..n {
        if !stripes.is_stripe_empty(i) {
            occupied.push(i);
        }
    }
    let total = stripes.total_len();

    pool.end_cycle();
    pool.shutdown();
    (n, occupied.len(), total, occupied)
}

/// Nothing is lost and nothing is duplicated, for a wide spread of batch sizes
/// against every worker count the pool supports.
///
/// This is the assertion that would have caught an index that wrapped: two
/// chunks landing in one stripe keeps `total` right, so it is checked together
/// with the touched-stripe count below rather than on its own.
#[test]
fn every_pushed_root_reaches_exactly_one_stripe() {
    for workers in [1usize, 2, 3, 4, 8] {
        for k in [1usize, 2, 3, 7, 8, 9, 15, 16, 17, 31, 100, 1000] {
            let (n, _touched, total, _) = push_and_measure(k, workers);
            assert_eq!(
                n,
                stripe_count_for(workers),
                "workers={workers}: the pool sized its stripes differently than \
                 `stripe_count_for` says"
            );
            assert_eq!(
                total, k,
                "workers={workers} k={k}: {total} entries reached the stripes out \
                 of {k} pushed"
            );
        }
    }
}

/// The chunk count never exceeds the stripe count, so `enumerate()`'s index is
/// always a real stripe and two chunks never collide.
///
/// The touched-stripe count is the observable: `publish_slice` masks its key,
/// so a chunk index of `n_stripes` would land back in stripe 0 and show up here
/// as one *fewer* stripe touched than there were chunks.
#[test]
fn the_chunk_count_never_exceeds_the_stripe_count() {
    for workers in [1usize, 2, 4, 8] {
        let n_stripes = stripe_count_for(workers);
        for k in 1..=20usize {
            let expected = expected_chunks(k, n_stripes);
            assert!(
                expected <= n_stripes,
                "workers={workers} k={k}: the arithmetic itself yields {expected} \
                 chunks for {n_stripes} stripes"
            );
            let (n, touched, total, occupied) = push_and_measure(k, workers);
            assert_eq!(n, n_stripes);
            assert_eq!(total, k);
            assert_eq!(
                touched, expected,
                "workers={workers} k={k}: {touched} stripes hold work but the \
                 chunking should have produced {expected} chunks (occupied: \
                 {occupied:?}). A shortfall means two chunks shared a stripe, \
                 which is an index that wrapped."
            );
            assert_eq!(
                occupied,
                (0..expected).collect::<Vec<_>>(),
                "workers={workers} k={k}: chunk `i` must land in stripe `i`, \
                 contiguously from 0"
            );
        }
    }
}

/// A batch smaller than the stripe count must not touch every stripe.
///
/// This is the *reason* the round-robin form was replaced: it took all 8 to 128
/// stripe mutexes to hand over two addresses, on a mutator thread, with a
/// worker's steal sweep possibly sitting on each one.
#[test]
fn a_small_batch_touches_only_as_many_stripes_as_it_has_entries() {
    for k in 1..=4usize {
        let (n, touched, total, _) = push_and_measure(k, 8);
        assert!(
            n >= Z_MARK_MIN_STRIPES && n <= Z_MARK_MAX_STRIPES,
            "stripe count {n} left its clamp"
        );
        assert_eq!(total, k);
        assert_eq!(
            touched, k,
            "a {k}-address batch must touch {k} stripes, not {n}"
        );
    }
}

/// Every participating stripe gets an equal share, which is the property the
/// round-robin was originally chosen for and which the chunking has to keep:
/// a worker whose own stripe is empty steals immediately, and a pool that
/// starts unbalanced spends its first phase stealing instead of marking.
#[test]
fn the_chunks_are_balanced_to_within_one_full_group() {
    let workers = 4;
    let n_stripes = stripe_count_for(workers);
    for k in [16usize, 17, 63, 64, 65, 1000] {
        let ctx: Arc<FlatContext> = Arc::new(FlatContext::default());
        let pool = ZMarkCoordinator::new(ctx, workers);
        pool.begin_cycle();
        let roots: Vec<u64> = (1..=k as u64).collect();
        assert_eq!(pool.push_roots(&roots), k);

        let stripes = pool.shared().stripes();
        let groups = n_stripes.min(k);
        let per_group = k.div_ceil(groups);

        let mut drained = 0usize;
        let mut sizes = Vec::new();
        for i in 0..stripes.stripe_count() {
            let mut out = Vec::new();
            let got = stripes.drain_from(i, &mut out, usize::MAX);
            if got > 0 {
                sizes.push(got);
            }
            drained += got;
        }
        assert_eq!(drained, k, "k={k}");
        for (i, size) in sizes.iter().enumerate() {
            assert!(
                *size <= per_group,
                "k={k}: chunk {i} holds {size}, above the {per_group} it was sized for"
            );
            assert!(*size >= 1, "k={k}: an empty chunk was published");
        }
        // Only the LAST chunk may be short; every earlier one is exactly
        // `per_group`. That is what "contiguous in discovery order" buys over
        // the round-robin, and it is what a future `per_group` off-by-one would
        // break first.
        for (i, size) in sizes.iter().enumerate().take(sizes.len().saturating_sub(1)) {
            assert_eq!(
                *size, per_group,
                "k={k}: chunk {i} is short but is not the last one"
            );
        }

        pool.end_cycle();
        pool.shutdown();
    }
}
