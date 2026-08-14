# Making ZGC concurrent and generational — the two properties that are still unimplemented

**Written 2026-08-13**, after
[`zgc-maturity-assessment-and-plan-20260813.md`](zgc-maturity-assessment-and-plan-20260813.md)
closed all five of its phases. That plan took the collector from six adopted
submodules to twelve, put a load barrier on a read path, and landed
stop-the-world compaction. It did **not** make the collector concurrent or
generational, and this document is about the difference between "off" and "not
built", because the two remaining properties are on opposite sides of it from
the two that landed.

---

## 1. Where the four properties actually stand

The distinction that matters here is **end-to-end reachability**: is there a
code path a running VM can take that exercises the property, or only components
that tests drive?

| property | status | evidence |
|---|---|---|
| **Parallel marking** | **Built, opt-in** | `CRATONVM_ZGC_PARMARK=<n>` reaches `mark_parallel_stw` from `collect_garbage` |
| **Compacting** | **Built, opt-in** | `CRATONVM_ZGC_RELOCATE=1` reaches `relocate_stw` from `collect_garbage`; returns a non-empty `PointerMap` and rewrites roots |
| **Concurrent** | **NOT BUILT** | `set_mark_active(true)` and `ZgcConcurrentMarkController::spawn` have **no non-test caller**. The driver exists, the ingress is wired and inert, and nothing starts a cycle |
| **Generational** | **NOT BUILT** | page ages, the card barrier and `ZGenerationScope` are computed inside `relocate_stw`, but `remembered_roots` has **no non-test caller** and there is no young-only collection. (`minor_collect` in `zgc.rs` belongs to the *simulation* half, not `ZgcRealHeap`.) |

So the first two are a flag away and the last two are a project. Saying "ZGC
isn't concurrent by default" would be wrong in a way this tree has been burned
by before: it is not off, it is absent.

**What already exists, and is the reason this is weeks and not months:**

* `gc/src/zgc/mark.rs` — the worker pool, striped queues, work stealing and the
  termination handshake. **Exercised against the real heap** since parallel STW
  marking landed, so it is no longer only test-driven.
* `gc/src/zgc_concurrent.rs` — the cycle driver, including the restart loop and
  the reference-processing re-drain. Complete; never spawned outside tests.
* `ZgcRealHeap: ZMarkContext` — the marking seam, including the collection
  overlay edge that adopting it exposed as missing.
* `ZgcRealHeap::satb_pre_barrier` — the mutator ingress, fed by
  `VmHeap::satb_barrier`, which every reference store in the VM already reaches.
  Armed by a flag nothing sets.
* `ZgcRealHeap::note_ref_store` + `remembered::ZRememberedSetTable` — the card
  barrier, fed by `GarbageCollector::write_barrier`. Same shape: wired, unarmed.

---

## 2. Phase C — concurrent marking

The plan's Phase 3 said this needed "a mark-start safepoint, a mutator write
barrier feeding `mark::ZMarkIngress`, and a decision on
`collect_garbage_with_finalizers`'s resurrection pass". The barrier is done.
Two remain, plus the piece that phase did not name.

### C1 — `ZgcMarkSafepoint` for the real VM *(the critical path)*

`ZgcConcurrentMarkParams` wants an `Arc<dyn ZgcMarkSafepoint>` with three
methods: `begin_mark_end_safepoint`, `flush_mutator_buffers`,
`end_mark_end_safepoint`. Nothing implements it against this VM.

The implementor must drive the VM's existing safepoint path — the one that
produces a `StopTheWorldToken` after `gc_barrier.wait_for_all()` — and hold the
token for the duration of the callback triple. **The token is `!Send` by
intent and the driver runs on its own thread**, so the implementor owns the
token on the thread that performs the stop; it cannot be passed through the
trait.

`flush_mutator_buffers` is the half that is a correctness requirement rather
than plumbing: an address sitting in a per-thread buffer is invisible to
`try_end_mark`, which then answers `Complete` with a live object unscanned.
Today there are no per-thread buffers — `satb_pre_barrier` pushes straight into
the shared ingress, taking one uncontended bucket mutex per store. That is
correct and it is the thing to fix second, not first (C4).

**Exit:** a concurrent cycle runs to `Complete` on a real heap with mutators
running, under a test that allocates and stores from several threads.

### C2 — Own the coordinator across cycles

`mark_parallel_stw` builds a `ZMarkCoordinator` per collection and joins it,
which is sound (see `ZHeapMarkBridge`) and pays a pool spawn every GC. A
concurrent cycle cannot do that: the pool must outlive the safepoint that
starts it.

This is the same ownership problem parallel marking dodged, and it has to be
solved properly now. `ZMarkCoordinator::new` takes `Arc<dyn ZMarkContext>` and
`ZgcRealHeap` is held **by value** inside `VmHeap`. The options, costed:

1. **`Arc<ZgcRealHeap>` inside `VmHeap::Zgc`.** Touches the enum and every arm
   that pattern-matches it (69 sites), but it is the honest fix and it is what
   lets the pool live on the heap.
2. **Keep the raw-pointer bridge and pin the heap.** Cheaper, but the soundness
   argument stops being "the bridge cannot outlive one `&self` call" and
   becomes "the heap is never moved", which nothing enforces.

**Recommend (1).** The bridge was justified for a call-scoped borrow; it is not
justified for a pool that outlives the call.

**Exit:** one worker pool per heap, surviving across collections; the
per-collection spawn disappears from `mark_parallel_stw` too.

### C3 — Decide the resurrection pass

`collect_garbage_with_finalizers` marks dead-but-finalizable objects and their
subtrees live *after* the main closure, so that "unmarked" means dead. Under a
concurrent marker that pass has no obvious home: it needs a complete mark, and
a concurrent mark is only complete inside the mark-end safepoint.

Two candidates, and this is a decision to make deliberately rather than
discover:

* run it inside the mark-end safepoint, before `try_end_mark`'s verdict — makes
  the pause longer by the finalizable subtree;
* feed the finalizable roots through `ZNonStrongRefHook::keep_alive`, which the
  driver already re-drains — keeps the pause short, but a resurrected subtree
  then extends the concurrent phase, and the restart budget must cover it.

The second is what `ZNonStrongRefHook` was designed for. Prefer it; measure the
restart count.

### C4 — Per-thread mark buffers

Replace the shared-ingress push with `ZMarkHandle::new_buffer` per mutator
thread, flushed at the mark-end safepoint. **Construct them with
`new_buffer`, never `ZMarkMutatorBuffer::new`** — the latter produces a
*detached* buffer, and a detached buffer dropped non-empty leaves objects
marked-and-unscanned, which is a use-after-free rather than a lost
optimisation, because the mark bit is what dedups them.

Pure throughput; do it after C1–C3 work.

**Exit for Phase C:** pause time falls measurably on a heap with a large live
set, and no suite regression.

---

## 3. Phase G — generational

Everything here already exists as *accounting*; what is missing is a cycle that
uses it.

### G1 — A young-only collection

`collect_garbage` marks the whole heap from the thread roots. A young cycle
marks only young pages, from the thread roots **plus** `remembered_roots`.

The pieces are in place: `ZGenerationScope::admits(addr)` decides membership,
`remembered_roots` supplies the old-to-young edges, and the card barrier keeps
the table current. What has to be written is the cycle itself and its sweep:
a young sweep must reclaim only within the scope, or it will free old objects
it never marked.

**The correctness hazard to plan around:** the card barrier is armed only once
a page is old, so edges created *before* the first promotion are not recorded.
The first young cycle after a promotion must therefore treat the newly-old page
as wholly dirty rather than trust an empty card set for it.

**Exit:** a young cycle collects a young-only workload with strictly less work
than a full cycle, and a test proves an object reachable only through an old
field survives it — the assertion that already exists for `remembered_roots`,
promoted to end-to-end.

### G2 — Promotion, and a real young space

Today "promotion" ages a logical grid cell. A real generational collector
*moves* survivors into an old space. That needs `ZPageAllocator`, which is the
one place this plan and the page-allocator question meet: **generational
promotion is the first thing that genuinely requires it**, because a logical
grid cannot separate young from old in address space.

Sequence it last, and treat the allocator swap as its own change with its own
measurement.

---

## 4. Sequencing, and what to do first

```
C1 safepoint ──┬─> C3 resurrection ─> [concurrent mark works]
C2 ownership ──┘                              │
                                              v
                              C4 per-thread buffers (throughput)

G1 young cycle ─> G2 promotion + ZPageAllocator (needs the allocator swap)
```

**C1 and C2 are independent and are both on the critical path.** C2 is the
larger diff and the smaller risk; C1 is the smaller diff and where a mistake is
a missed object. Doing C2 first gives C1 somewhere to put the pool.

**Phase C before Phase G**, for the reason the original plan gave for
concurrency before relocation: a bug in concurrent marking costs throughput or
surfaces as a missed object under a test, while a bug in a young cycle's sweep
frees live old objects.

---

## 5. What this plan does not do

**It does not propose making anything default-on.** Every property here exits
on a measurement, and the two properties that *are* built —
`CRATONVM_ZGC_PARMARK` and `CRATONVM_ZGC_RELOCATE` — should have their gauntlet
numbers in hand before a third and fourth join them. Flipping four switches at
once produces one result and four candidate explanations.
