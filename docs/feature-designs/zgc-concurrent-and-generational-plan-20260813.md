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
| **Concurrent** | **PARTLY BUILT — the driver runs, the mutators do not** | Since 2026-08-14 `ZgcConcurrentMarkController` drives every collection against `ZgcRealHeap`, with its restart loop, mark-end handshake and `mark_set_complete` verdict. What is not concurrent is the mutator half: the cycle runs **at a safepoint**, so `ZgcNoMutatorSafepoint` is legitimately a no-op and `set_mark_active(true)` still has no non-test caller. C1 and C2 below are what make it concurrent |
| **Generational** | **NOT BUILT** | page ages, the card barrier and `ZGenerationScope` are computed inside `relocate_stw`, but `remembered_roots` has **no non-test caller** and there is no young-only collection. (`minor_collect` in `zgc.rs` belongs to the *simulation* half, not `ZgcRealHeap`.) |

So the first two are a flag away, generational is a project, and concurrency
is now half a project: the machinery runs, the mutators are still stopped while
it does. Saying "ZGC isn't concurrent by default" would be wrong in a way this
tree has been burned by before — it is not off, and it is no longer wholly
absent either. The precise missing thing is a `ZgcMarkSafepoint` that really
stops mutators and a pool that outlives it.

**What already exists, and is the reason this is weeks and not months:**

* `gc/src/zgc/mark.rs` — the worker pool, striped queues, work stealing and the
  termination handshake. **Exercised against the real heap** since parallel STW
  marking landed, so it is no longer only test-driven.
* `gc/src/zgc_concurrent.rs` — the cycle driver, including the restart loop and
  the reference-processing re-drain. **Driving every collection since
  2026-08-14**, at a safepoint.
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

**Narrowed 2026-08-14.** The driver, its restart loop and its verdict are now
exercised on the real heap every cycle, so this is no longer "wire up an
unproven component" — it is "replace one no-op implementation with a real one".
`ZgcNoMutatorSafepoint` is correct while the cycle is stop-the-world and becomes
wrong the moment it is not, which makes it the single switch this phase turns.


`ZgcConcurrentMarkParams` wants an `Arc<dyn ZgcMarkSafepoint>` with three
methods: `begin_mark_end_safepoint`, `flush_mutator_buffers`,
`end_mark_end_safepoint`. The only implementation reaching the real heap is
`ZgcNoMutatorSafepoint`, which stops nothing and flushes nothing.

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

### C5 — Make the marker actually scale *(new 2026-08-14, and it is the one with a number)*

**Adding workers makes the pause WORSE, and even one worker costs.** Measured
on `probes/BigLive.java` (1,000,088 live objects, `-Xmx1500m`, relocation off),
three interleaved reps, mean of the last five cycles each:

| workers | mean pause | vs serial |
|---:|---:|---:|
| 0 — the bespoke serial loop | **95.2 ms** | — |
| 1 — driven, one worker | 124.9 ms | **+31%** |
| 2 | 237.4 ms | +149% |
| 4 | 240.6 ms | +153% |
| 8 | ~304 ms | +219% |

Two separate costs are stacked here and they want separate fixes:

* **A fixed ~30% for driving at all**, visible at one worker where no
  contention can exist. Per-cycle pool and driver-thread spawn, plus the
  striped-queue path replacing a plain `Vec` work stack.
* **Contention on top**, which is what makes the curve rise monotonically
  rather than flatten. A fixed overhead would flatten and then improve as the
  work divided; this does not. Two candidates, both per-object and both hot:
  `ZMarkStripeSet` takes a `Mutex` per publish and per steal, and `visit_refs`
  resolves a class layout per object through the class-manager `RwLock` — a
  contended reader lock on one cache line does not scale even though it never
  blocks.

**This is why `Z_PARMARK_DEFAULT_WORKERS` is 0.** The driver stays reachable,
correct and covered end-to-end by
`the_concurrent_mark_driver_drives_a_real_collection`; it is simply not what a
user's pauses pay for until this lands.

**Method note, because it nearly went the other way.** A single un-interleaved
run of the one-worker arm measured 95.7 ms — free — and a default of `1` was
briefly justified on it. Three interleaved reps put it at 124.9 ms with a
spread of 2.7 ms; the original figure was noise on a box that had just finished
a build. Interleave, or do not compare.

**Exit:** four workers beat zero on the table above. Until then the honest
statement is that this collector's marking is *drivable*, not *parallel*.

**Exit for Phase C:** pause time falls measurably on a heap with a large live
set, and no suite regression. **Owed by C5 first** — the pause has to stop
rising with worker count before concurrency can lower it, and C1's concurrent
phase would otherwise inherit the same contention with mutators running
alongside it.

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
