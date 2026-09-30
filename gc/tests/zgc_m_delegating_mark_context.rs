// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The seam between wave 1's `ZMarkContext::claim_child` and wave 2's
//! `ZMarkContext::visit_refs_chunked` — two defaulted methods added by two
//! lanes that never saw each other's work.
//!
//! # What this file is for
//!
//! Three properties, none of which any existing test states:
//!
//! 1. **A defaulted method is inherited, not forwarded.** A *delegating*
//!    `ZMarkContext` — one that wraps another and forwards the required
//!    methods — silently reverts to the trait's unsplit `visit_refs_chunked`
//!    unless it forwards that one too. Nothing fails to compile, nothing logs,
//!    and the mark set is byte-identical; the only observable is
//!    `ZMarkStats::ref_chunks` stuck at zero. `gc/src/zgc.rs`'s
//!    `ZHeapMarkBridge` is exactly this shape and is exactly this wrong — see
//!    `docs/internal/zgc-round-20260920/handoff-m-mark-bridge-drops-chunked-scan.md`.
//!    The bridge is private, so the property is stated here in the abstract,
//!    where it is testable.
//!
//! 2. **A half-scanned object is work even when the local stack is empty.**
//!    `ZMarkWorker::run_cycle` used to ask `!self.local.is_empty()` and now
//!    asks `has_work()`, which also covers the in-progress cursor. The two
//!    answers differ only for an object whose chunks push *nothing* — an array
//!    of nulls, or of already-marked referents, which is the steady state on a
//!    re-scan. That is a narrow window and it is a use-after-free: the worker
//!    offers termination, the fixed point is declared over a live subgraph, and
//!    the sweep frees it. `a_hub_whose_chunks_push_nothing_is_still_work`
//!    constructs precisely that object.
//!
//! 3. **Chunking changes nothing under work stealing.** The in-progress cursor
//!    lives in a worker-private field that no stripe and no other worker can
//!    see, so a layered graph marked by several workers is where a lost cursor
//!    would show up as a missing subtree rather than as a missing object.
//!
//! Everything here is asserted on the **mark set**, never on timing.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use cratonvm_gc::zgc::mark::{ZMarkContext, ZMarkCoordinator, Z_MARK_DRAIN_BUDGET};

/// One restart is budgeted rather than zero: no mutator is running here, but
/// zero would turn any legitimate mark-end flush into an "INCOMPLETE mark set"
/// verdict, which is the one thing these tests must be able to distinguish
/// from a real loss.
const RESTART_BUDGET: usize = 8;

/// `Z_REMAPPED`, spelled as a literal so this file has no opinion about the
/// colour encoding. The engine does not branch on it.
const GOOD_MASK: u64 = 1 << 44;

// ---------------------------------------------------------------------------
// The context under test
// ---------------------------------------------------------------------------

/// An adjacency-map context that splits every object's out-edges into pieces
/// of at most `chunk_limit`.
///
/// Deliberately *not* `mark::TestMarkContext`: that one is the engine's own
/// fixture and several of these tests need a second, differently-wrapped
/// instance over the identical graph, which is easier to reason about from a
/// type this file owns outright.
struct Chunky {
    graph: HashMap<u64, Vec<u64>>,
    /// `None` keeps the trait's inherited body (one `visit_refs`, never
    /// splits). `Some(n)` reports at most `n` children per call.
    chunk_limit: Option<usize>,
    marked: Mutex<HashSet<u64>>,
}

impl Chunky {
    fn new(graph: HashMap<u64, Vec<u64>>, chunk_limit: Option<usize>) -> Self {
        if let Some(n) = chunk_limit {
            assert!(n > 0, "a zero chunk limit would never finish an object");
        }
        Chunky {
            graph,
            chunk_limit,
            marked: Mutex::new(HashSet::new()),
        }
    }

    fn marked_sorted(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self
            .marked
            .lock()
            .expect("poisoned")
            .iter()
            .copied()
            .collect();
        v.sort_unstable();
        v
    }
}

impl ZMarkContext for Chunky {
    fn good_mask(&self) -> u64 {
        GOOD_MASK
    }
    fn try_mark(&self, addr: u64) -> bool {
        self.marked.lock().expect("poisoned").insert(addr)
    }
    fn is_marked(&self, addr: u64) -> bool {
        self.marked.lock().expect("poisoned").contains(&addr)
    }
    fn visit_refs(&self, addr: u64, f: &mut dyn FnMut(u64)) {
        if let Some(children) = self.graph.get(&addr) {
            for &c in children {
                f(c);
            }
        }
    }
    fn visit_refs_chunked(
        &self,
        addr: u64,
        cursor: usize,
        budget: usize,
        f: &mut dyn FnMut(u64),
    ) -> Option<usize> {
        let Some(limit) = self.chunk_limit else {
            // Exactly the trait's provided body.
            if cursor == 0 {
                self.visit_refs(addr, f);
            }
            return None;
        };
        let Some(children) = self.graph.get(&addr) else {
            return None;
        };
        let start = cursor.min(children.len());
        let end = start
            .saturating_add(limit.min(budget.max(1)))
            .min(children.len());
        for &c in &children[start..end] {
            f(c);
        }
        if end >= children.len() {
            None
        } else {
            // Strictly greater than `cursor`: `start == cursor` whenever
            // `cursor < len`, and at least one element was consumed.
            Some(end)
        }
    }
    fn is_in_heap(&self, addr: u64) -> bool {
        self.graph.contains_key(&addr)
    }
}

// ---------------------------------------------------------------------------
// The two wrapper shapes
// ---------------------------------------------------------------------------

/// The `ZHeapMarkBridge` shape: forwards only the **required** methods and
/// lets every defaulted one fall through to the trait.
struct ForwardsRequiredOnly(Arc<Chunky>);

impl ZMarkContext for ForwardsRequiredOnly {
    fn good_mask(&self) -> u64 {
        self.0.good_mask()
    }
    fn try_mark(&self, addr: u64) -> bool {
        self.0.try_mark(addr)
    }
    fn is_marked(&self, addr: u64) -> bool {
        self.0.is_marked(addr)
    }
    fn visit_refs(&self, addr: u64, f: &mut dyn FnMut(u64)) {
        self.0.visit_refs(addr, f)
    }
    fn is_in_heap(&self, addr: u64) -> bool {
        self.0.is_in_heap(addr)
    }
}

/// The same wrapper with the one extra line the bridge is missing.
struct ForwardsChunkedToo(Arc<Chunky>);

impl ZMarkContext for ForwardsChunkedToo {
    fn good_mask(&self) -> u64 {
        self.0.good_mask()
    }
    fn try_mark(&self, addr: u64) -> bool {
        self.0.try_mark(addr)
    }
    fn is_marked(&self, addr: u64) -> bool {
        self.0.is_marked(addr)
    }
    fn visit_refs(&self, addr: u64, f: &mut dyn FnMut(u64)) {
        self.0.visit_refs(addr, f)
    }
    fn is_in_heap(&self, addr: u64) -> bool {
        self.0.is_in_heap(addr)
    }
    fn visit_refs_chunked(
        &self,
        addr: u64,
        cursor: usize,
        budget: usize,
        f: &mut dyn FnMut(u64),
    ) -> Option<usize> {
        self.0.visit_refs_chunked(addr, cursor, budget, f)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// `1` is the hub; `2 ..= 1 + n` are childless leaves.
fn hub_graph(n: u64) -> HashMap<u64, Vec<u64>> {
    let mut g: HashMap<u64, Vec<u64>> = HashMap::new();
    let children: Vec<u64> = (2..=1 + n).collect();
    for &c in &children {
        g.insert(c, Vec::new());
    }
    g.insert(1, children);
    g
}

/// Run one mark to completion over `ctx` and return `(ref_chunks,
/// objects_scanned, mark_set_complete)`.
fn mark(ctx: Arc<dyn ZMarkContext>, workers: usize, roots: &[u64]) -> (u64, u64, bool) {
    let pool = ZMarkCoordinator::new(ctx, workers);
    pool.begin_cycle();
    assert_eq!(
        pool.push_roots(roots),
        roots.len(),
        "every distinct root must be newly claimed"
    );
    let report = pool.mark_to_completion(RESTART_BUDGET);
    pool.end_cycle();
    let out = (
        report.stats.ref_chunks,
        report.stats.objects_scanned,
        report.mark_set_complete(),
    );
    pool.shutdown();
    out
}

// ---------------------------------------------------------------------------
// 1. The delegating-wrapper hazard
// ---------------------------------------------------------------------------

/// A wrapper that forwards `visit_refs` and stops **silently loses the split**.
///
/// This is the whole of the `ZHeapMarkBridge` finding, stated where it can be
/// executed. Both arms must produce the identical mark set — that is why the
/// mistake is silent — and they must differ in `ref_chunks`, which is the only
/// thing that can tell them apart.
///
/// The consequence in `gc/src/zgc.rs` is that
/// `CRATONVM_ZGC_MARK_CTX_DIRECT=0`, documented as an A/B over one indirect
/// call per method, is since wave 2 also an A/B over *whether huge reference
/// arrays are interruptible at all*.
#[test]
fn a_wrapper_that_forwards_only_visit_refs_silently_loses_the_split() {
    const HUB_CHILDREN: u64 = 200;
    const LIMIT: usize = 7;

    let inner_bare = Arc::new(Chunky::new(hub_graph(HUB_CHILDREN), Some(LIMIT)));
    let bare: Arc<dyn ZMarkContext> = Arc::new(ForwardsRequiredOnly(inner_bare.clone()));
    let (bare_chunks, bare_scanned, bare_ok) = mark(bare, 2, &[1]);

    let inner_full = Arc::new(Chunky::new(hub_graph(HUB_CHILDREN), Some(LIMIT)));
    let full: Arc<dyn ZMarkContext> = Arc::new(ForwardsChunkedToo(inner_full.clone()));
    let (full_chunks, full_scanned, full_ok) = mark(full, 2, &[1]);

    assert!(bare_ok && full_ok, "both marks must converge");

    // The mark sets are identical. That is not a happy accident, it is the
    // reason the omission survives review: there is no wrong answer to see.
    assert_eq!(
        inner_bare.marked_sorted(),
        inner_full.marked_sorted(),
        "the two wrappers must reach the same objects"
    );
    assert_eq!(
        inner_full.marked_sorted().len() as u64,
        HUB_CHILDREN + 1,
        "the hub plus every child"
    );
    assert_eq!(bare_scanned, full_scanned, "and finish the same objects");
    assert_eq!(full_scanned, HUB_CHILDREN + 1);

    // ...and the ONLY difference is the counter.
    assert_eq!(
        bare_chunks, 0,
        "a wrapper that does not forward `visit_refs_chunked` inherits the \
         trait's unsplit body — this is `ZHeapMarkBridge`, and it is why \
         CRATONVM_ZGC_MARK_CTX_DIRECT=0 is no longer a dispatch-only A/B"
    );
    // 200 children at 7 per call is 29 calls, 28 of which do not finish the hub.
    assert_eq!(
        full_chunks,
        (HUB_CHILDREN as usize).div_ceil(LIMIT) as u64 - 1,
        "one continuation per call that did NOT finish the hub"
    );
}

// ---------------------------------------------------------------------------
// 2. `has_work()` vs `!local.is_empty()`
// ---------------------------------------------------------------------------

/// A half-scanned object whose chunks push **nothing** is still work.
///
/// # The construction, and why it is the only one that discriminates
///
/// `ZMarkWorker::run_cycle` decides whether to re-enter the drain or fall
/// through to the termination handshake. Before wave 2 it asked
/// `!self.local.is_empty()`; it now asks `has_work()`, which also covers the
/// in-progress cursor. Those two answers agree for every object whose chunks
/// push children onto the local stack — which is every object in the engine's
/// own chunking tests, so none of them can tell the two apart.
///
/// They disagree for an object whose chunks push nothing: an array of nulls,
/// or one whose referents are all already marked, which is the ordinary state
/// of a re-scanned array. This hub is that object. Its first
/// chunks past `Z_MARK_DRAIN_BUDGET` are entirely null slots, so after one
/// drain the worker holds an **empty local stack** and a cursor parked
/// mid-object; its last chunk holds the only real children in the graph.
///
/// With `has_work()` the worker comes straight back in and finishes the hub.
/// With `!local.is_empty()` it would offer termination, the fixed point would
/// be declared with the hub's tail untraced, `mark_to_completion` would report
/// a **complete** mark set, and the sweep would free `PAYLOAD`. The assertion
/// is therefore on the mark set and not on a counter.
///
/// One worker, so nothing can rescue the hub by stealing it — an in-progress
/// object is deliberately not stealable.
#[test]
fn a_hub_whose_chunks_push_nothing_is_still_work() {
    const LIMIT: usize = 4;
    const PAYLOAD: u64 = 9_000;

    // Past the drain budget, so the drain must return with the cursor parked
    // and `run_cycle` must decide to come back in. A handful past rather than
    // exactly one past, so the "spanned more than one drain call" assertion
    // below is a strict inequality rather than a boundary.
    let chunks = Z_MARK_DRAIN_BUDGET + 8;
    let mut children: Vec<u64> = vec![0; (chunks - 1) * LIMIT];
    // The final chunk: the only reachable object in the whole graph.
    children.extend([PAYLOAD, 0, 0, 0]);
    assert_eq!(children.len(), chunks * LIMIT);

    let mut g: HashMap<u64, Vec<u64>> = HashMap::new();
    g.insert(PAYLOAD, Vec::new());
    g.insert(1, children);

    let inner = Arc::new(Chunky::new(g, Some(LIMIT)));
    let ctx: Arc<dyn ZMarkContext> = inner.clone();
    let (ref_chunks, scanned, complete) = mark(ctx, 1, &[1]);

    assert!(complete, "the mark must converge");
    assert_eq!(
        inner.marked_sorted(),
        vec![1, PAYLOAD],
        "the worker abandoned a half-scanned object with an EMPTY local stack: \
         it offered termination while still owning the hub's tail, so the \
         fixed point closed over a live object"
    );
    assert_eq!(
        scanned, 2,
        "`objects_scanned` counts objects FINISHED, not chunks"
    );
    assert_eq!(
        ref_chunks as usize,
        chunks - 1,
        "the fixture must really have parked a cursor past the drain budget, \
         or it asserts nothing about re-entry"
    );
    assert!(
        ref_chunks as usize > Z_MARK_DRAIN_BUDGET,
        "and must have spanned more than one drain call"
    );
}

// ---------------------------------------------------------------------------
// 3. Chunking under work stealing
// ---------------------------------------------------------------------------

/// A layered graph, marked with and without chunking, by a pool big enough to
/// steal. The mark sets must be equal.
///
/// The in-progress cursor is worker-private: no stripe holds it and no other
/// worker can claim the object (its mark bit is already set). So a graph deep
/// enough that workers genuinely rebalance is where a dropped cursor shows up
/// as a missing *subtree* rather than a missing object, and that is the shape
/// this adds over the engine's own flat-hub test.
#[test]
fn chunked_and_unchunked_agree_under_work_stealing() {
    const HUBS: u64 = 8;
    const FANOUT: u64 = 100;
    const DEPTH_FANOUT: u64 = 3;

    let build = || -> HashMap<u64, Vec<u64>> {
        let mut g: HashMap<u64, Vec<u64>> = HashMap::new();
        let mut next: u64 = 100;
        let mut hubs: Vec<u64> = Vec::new();
        for _ in 0..HUBS {
            let hub = next;
            next += 1;
            let mut kids: Vec<u64> = Vec::new();
            for _ in 0..FANOUT {
                let kid = next;
                next += 1;
                let mut grandkids: Vec<u64> = Vec::new();
                for _ in 0..DEPTH_FANOUT {
                    let gk = next;
                    next += 1;
                    g.insert(gk, Vec::new());
                    grandkids.push(gk);
                }
                g.insert(kid, grandkids);
                kids.push(kid);
            }
            g.insert(hub, kids);
            hubs.push(hub);
        }
        g.insert(1, hubs);
        g
    };

    let plain = Arc::new(Chunky::new(build(), None));
    let (plain_chunks, plain_scanned, plain_ok) = mark(plain.clone(), 4, &[1]);

    let chunky = Arc::new(Chunky::new(build(), Some(7)));
    let (chunky_chunks, chunky_scanned, chunky_ok) = mark(chunky.clone(), 4, &[1]);

    assert!(plain_ok && chunky_ok, "both marks must converge");
    assert_eq!(
        chunky.marked_sorted(),
        plain.marked_sorted(),
        "chunking must not change what is reachable, however the work was \
         split between the stealing workers"
    );
    let expected = 1 + HUBS + HUBS * FANOUT + HUBS * FANOUT * DEPTH_FANOUT;
    assert_eq!(plain.marked_sorted().len() as u64, expected);
    assert_eq!(
        plain_scanned, expected,
        "every object finished exactly once"
    );
    assert_eq!(chunky_scanned, expected);

    assert_eq!(
        plain_chunks, 0,
        "the inherited visit never parks a cursor — if this is non-zero the \
         `None` chunk_limit arm has stopped being the trait's body"
    );
    assert!(
        chunky_chunks > 0,
        "the chunked arm must actually have split, or the equality above is \
         vacuous"
    );
}
