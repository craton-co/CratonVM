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
| **Concurrent** | **BUILT since 2026-08-16 — the mutators really do run** | `maybe_gc` opens a cycle at a brief STW once allocation crosses `CRATONVM_ZGC_CONC_START`% of the collection threshold; the pool traces the closure while every mutator runs; the next collection's pause replays the SATB ingress, re-scans the roots and certifies the mark set. `set_mark_active(true)` has a production caller, allocation is BLACK during a cycle, and `--verbose:gc` says `mark=concurrent`. See §2 for what landed and how it differs from C1's expected shape |
| **Generational** | **NOT BUILT** | page ages, the card barrier and `ZGenerationScope` are computed inside `relocate_stw`, but `remembered_roots` has **no non-test caller** and there is no young-only collection. (`minor_collect` in `zgc.rs` belongs to the *simulation* half, not `ZgcRealHeap`.) |

**Updated 2026-08-16.** Concurrency landed. The rest of this section is kept as
written on 2026-08-13 so the diff between what was planned and what was built
stays legible; §2's per-item headers say which of C1–C5 are closed and which
are not, and C1 closed in a **different shape** than it was specified in — for
an architectural reason that is worth reading before the next collector change
proposes a background thread that takes a safepoint.

Generational is still a project and is untouched by this.

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

### C1 — a real mark safepoint — **CLOSED 2026-08-16, in a different shape**

**What was built, and why it is not what this item asked for.** This item
specified an `Arc<dyn ZgcMarkSafepoint>` whose implementor drives the VM's STW
path, so that `ZgcConcurrentMarkController`'s *driver thread* could take the
mark-end pause itself. That cannot be built in this VM, and the reason is
structural rather than a matter of effort:

* `GcBarrier::request_stw_counted_with_live_blocked` is keyed on a **registered
  `ThreadId`**;
* `stw_take_over_and_wait` forcibly stops in-JIT peers using the *initiator's*
  own JIT context and conservatively scans them;
* `StopTheWorldToken` is `!Send` on purpose, and this item's own text
  acknowledges that by making the implementor own it on the stopping thread.

A GC background thread satisfies none of the three. G1 hit the same wall and
answered it by putting **both** phase boundaries on mutators —
`g1_concurrent_mark_cycle` opens the cycle, `g1_final_remark_cleanup` closes it
— with the background worker doing only the part that needs no safepoint.

ZGC now does the same:

```text
  mark start   MUTATOR, brief STW   zgc_concurrent_mark_cycle -> start_concurrent_mark
  concurrent   MARK WORKERS         the pool traces; every mutator runs
  mark end     MUTATOR, the GC STW  collect_garbage -> finish_concurrent_mark
  sweep        MUTATOR, same STW    the unchanged collect_garbage tail
```

`ZgcMarkSafepoint` and `ZgcNoMutatorSafepoint` are untouched and still correct
for the stop-the-world driver path in `mark_with_controller_stw`. What this VM
does not have is a caller for a *non*-no-op implementation, and after this
change it is clear that it never will — that is worth knowing before the next
collector change proposes one.

**Two things SATB needed beyond the pre-write barrier, and both are in
`gc/src/zgc.rs`:**

* **Allocation is BLACK during a cycle** (`allocate_black_if_marking`). The
  concurrent mark closes over the root set *as it was at mark start*; nothing
  allocated afterwards is in it, so without this the sweep frees the entire
  live set a busy allocator produced while the marker ran. It is set AFTER the
  header write, because `ptr::write` of an `ObjectHeader` clobbers the flags
  byte and a bit set before it would be erased silently.
* **The roots are re-scanned inside the mark-end pause.** A thread created
  during the concurrent phase has a stack the mark-start scan never saw.

**And one hole the call-site census had:** `System.arraycopy` copies a
reference array element by element through `NativeContext::set_array_element`,
and no pre-write barrier sat anywhere above that path. Both accessors
(`ZgcRealHeap::set_field` and `set_array_element`) now publish the overwritten
reference themselves, so coverage is a property of the ONE store path rather
than of a census that has to stay complete forever. **G1 has the same hole and
it is still open there** — G1's concurrent marking is reachable today, so this
is a live defect on that backend, not a hypothetical.

**Fails closed.** If the mark-end handshake cannot certify a complete mark set,
`finish_concurrent_mark` returns `None`, `collect_garbage` clears every mark bit
and marks from scratch. A sweep against an uncertified mark set is a
use-after-free; this is the same fail-closed discipline
`mark_with_controller_stw` already had.

**Kill switch:** `CRATONVM_ZGC_CONC_START=0` (or `CRATONVM_GC=-zgc-conc-start`).

<details>
<summary>The 2026-08-13 specification, kept for the diff</summary>

#### C1 (as specified) — `ZgcMarkSafepoint` for the real VM *(the critical path)*

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

</details>

**Exit MET.** `several_mutator_threads_run_during_the_concurrent_phase` in
`gc/src/zgc.rs` runs four mutator threads that allocate and store into the live
set for the whole concurrent phase, then asserts `cycles_completed == 1` — not
merely that the graph survived, which is satisfied by the feature being
switched off and is the vacuous green this change had available to it.

### C2 — an owner for the coordinator — **CLOSED 2026-08-16, option (1)**

`VmHeap::Zgc` holds `Arc<ZgcRealHeap>`. That is the "honest fix" this item
recommended, and it cost far less than the 69-site estimate: `Arc<T>` derefs to
`T` and `ZgcRealHeap` has **no `&mut self` method**, so every existing
`VmHeap::Zgc(h) => h.method()` compiles unchanged. The heap is then handed to
the engine as the `Arc<dyn ZMarkContext>` it already implements — no bridge, no
`unsafe`, and no "the heap is never moved" argument.

One correction to this item's framing: the pool is built at mark start and
**dropped at mark end**, not kept across cycles. A coordinator parked on the
heap would hold `Arc<ZgcRealHeap>`, i.e. a reference cycle, and the heap and its
worker threads would never be freed. Dropping it at mark end breaks that cycle
at a point the collector controls, and `ZMarkCoordinator::drop` stops and JOINS
every worker there — so no thread holding a clone of the heap outlives the
cycle. The cost is one pool spawn per *concurrent cycle*, which is strictly
fewer than the per-*collection* spawn `mark_parallel_stw` was already paying.

<details>
<summary>The 2026-08-13 specification, kept for the diff</summary>

#### C2 (as specified) — Own the coordinator across cycles

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

</details>

**Exit MET in substance, not in letter.** One pool per *cycle* rather than per
heap, for the ownership reason above. `mark_parallel_stw` keeps its own
per-collection pool: it is the fallback path now, and giving it a shared pool
would put a live coordinator on the heap for exactly the reason this item's
option (1) exists to avoid.

### C3 — the resurrection pass — **DECIDED 2026-08-16: the first option**

It runs inside the mark-end safepoint, before the sweep, exactly where it
already was. The second option — feeding the finalizable roots through
`ZNonStrongRefHook::keep_alive` so the driver re-drains them — was written for
a design in which the driver thread owns the mark-end pause. It does not, and
cannot (see C1), so there is no shorter pause to protect: the resurrection pass
and the sweep are already in the same stop-the-world block, and moving the pass
into a hook would relocate work from one half of that block to the other while
adding a phase.

The cost this item warned about is real and is accepted: the pause is longer by
the finalizable subtree. What made that acceptable is what changed around it —
the *strong* closure, which is the part that scales with the live set, has left
the pause entirely.

<details>
<summary>The 2026-08-13 specification, kept for the diff</summary>

#### C3 (as specified) — Decide the resurrection pass

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

</details>

### C4 — per-thread mark buffers — **PARTLY SUPERSEDED 2026-08-16**

The problem this item names — SATB work sitting in a queue nobody is draining —
turned out to have a larger half than the per-store mutex, and the larger half
is fixed. `ZgcRealHeap::hand_satb_batch_to_the_marker` moves the accumulated
ingress into the marker's own stripes every `Z_SATB_HANDOFF_INTERVAL` (8192)
publications, and **re-arms the pool if it has already terminated** — without
that, the first time the marker caught up with the graph it stopped for good and
every SATB reference published afterwards waited for the pause. It also bounds
the ingress, which was otherwise growing with the reference-store count rather
than with the live set and appeared in no heap figure the VM reports.

What is NOT done is the per-thread buffer itself. `satb_pre_barrier` is reached
through `VmHeap::satb_barrier` with no thread context at all, so a real
`ZMarkHandle::new_buffer` per mutator needs thread-keyed state on the heap
first. It is still worth doing and it is still pure throughput.

<details>
<summary>The 2026-08-13 specification, kept for the diff</summary>

#### C4 (as specified) — Per-thread mark buffers

Replace the shared-ingress push with `ZMarkHandle::new_buffer` per mutator
thread, flushed at the mark-end safepoint. **Construct them with
`new_buffer`, never `ZMarkMutatorBuffer::new`** — the latter produces a
*detached* buffer, and a detached buffer dropped non-empty leaves objects
marked-and-unscanned, which is a use-after-free rather than a lost
optimisation, because the mark bit is what dedups them.

Pure throughput; do it after C1–C3 work.

</details>

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

## 2b. What concurrent marking actually measured — 2026-08-16

Interleaved, 3 reps per arm, 8-core Azure box, JIT on (so relocation was
refused throughout, `relocation_skipped_jit`), `probes/ZgcConcMarkProbe.java`
at `-Xmx900m` and `probes/ZgcConcMarkThreadsProbe.java` at `-Xmx1200m`.
`mark=` and `cycles_started` were read on every row, so no arm is a
did-it-even-run guess.

| probe | arm | cycles | mean pause | median | wall clock |
|---|---|---:|---:|---:|---:|
| single-threaded | stop-the-world | 6 | 370 ms | 366 ms | 5.35 s |
| single-threaded | concurrent, 1 worker | **12** | **230 ms** | 209 ms | 8.31 s |
| single-threaded | concurrent, 2 workers | **12** | 254 ms | 233 ms | 9.70 s |
| 8 mutator threads | stop-the-world | 3 | 609 ms | 599 ms | 11.8 s |
| 8 mutator threads | concurrent, 1 worker | 5 | 388 ms | 325 ms | 16.8 s |
| 8 mutator threads | concurrent, 2 workers | 5 | **253 ms** | 261 ms | 16.2 s |

**Per-cycle pause falls 38–58%.** That is the property Phase C was written for
and it is real: every concurrent row reports `mark=concurrent` on every cycle,
and the multi-threaded arms report `black_allocations=16.5M`,
`satb_replayed=4.1M`, `BAD=0` — the graph survived eight concurrent mutators
with its self-tags intact.

**Two things beside it say this is not a default.**

* **The cycle count roughly doubles** (6 → 12, 3 → 5). Everything allocated
  after mark start is floating garbage for that cycle, so each collection
  reclaims less and the next arrives sooner. On the single-threaded probe that
  turns a 38% per-cycle win into a **worse total pause** (2.22 s → 2.76 s); on
  the multi-threaded one the total still improves (1.83 s → 1.27 s). A pause
  measurement that quoted only the per-cycle figure would have hidden that,
  which is why the cycle count is in the table.
* **Wall clock rises 37–55%.** Two causes the arms can separate only partly:
  the mark workers compete with mutators that already saturate the box (1
  worker is cheaper than 2 on the single-threaded probe, where the mutator is
  alone; 2 is cheaper than 1 on the multi-threaded one, where the marker has to
  keep up), and the store and allocation paths grew work — 22.4M allocate-black
  claims and 1.4M SATB publications on the single-threaded probe.

**So it ships behind `CRATONVM_ZGC_CONC_START=60` and the default is `0`.**
This tree shipped a ZGC marking feature default-ON verified only for
correctness once already — parallel STW marking, 2026-08-14, +31% at one worker
and +153% at four, reverted the same day. That lesson is written down in this
very file (§C5's method note); this is it being followed rather than quoted.

**What would move the default**, in the order the numbers point at:

1. **The floating-garbage cost**, which is the one that turns a pause win into
   a total-pause loss. A generational cycle (Phase G) is the structural answer;
   a cheaper one is to start the cycle later, since the window only has to be
   long enough to trace the live set once.
2. **The per-allocation and per-store telemetry.** `conc_black_allocations` and
   `mark_ingress_pushes` are `fetch_add`s on shared cache lines taken tens of
   millions of times per run. Both are only counters; the second is also the
   handoff trigger, and both are on the hottest paths in the VM.
3. **C5's contention**, unchanged and still unaddressed: `ZMarkStripeSet` takes
   a mutex per publish and per steal, and `visit_refs` clones an `Arc` out of an
   `RwLock` for the reference skip set on **every object**.

**Method note, again.** The first run of this measurement reported no
difference between the arms — and it was measuring nothing: at `-Xmx1500m` the
collection threshold is ~1125 MB and the whole workload allocated ~360 MB, so
neither arm collected once. `cycles_started=0` in the summary is what caught
it; a pause table alone would have read as "concurrency does not help". The
second run then showed `cycles_started=0` on the *single-threaded* probe only,
which is how the JIT trigger gap (below) was found. **Put the engagement
counter next to the number, or the number is not evidence.**

**The JIT trigger gap, found by that counter.** `maybe_gc` is the
*interpreter's* allocation hook. A JIT-compiled allocation loop never reaches
it — `jit_newarray` calls `heap.try_alloc_array_full`, which succeeds until the
heap is full, so a compiled `new byte[128]` loop consults no occupancy
predicate at all and its collections arrive by allocation *failure*. The
multi-threaded probe engaged only because its peers still run interpreted code.
`jit_new_object` and `jit_newarray` now carry the check themselves. The JIT
safepoint poll is not an alternative: `emit_safepoint_poll` fires only once
`stw_requested` is set, which is a consequence of a collection rather than a
cause of one.

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
