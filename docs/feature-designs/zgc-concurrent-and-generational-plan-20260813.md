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
| **Generational** | **BUILT since 2026-08-17, opt-in** | `CRATONVM_ZGC_GENERATIONAL=1` makes every collection between two whole-heap ones a young cycle: the old generation is pre-marked and never traced, the remembered set supplies the old-to-young roots, and the sweep ages and promotes. The split is by **object** age, not page age — see §3 for why the page grid cannot do it. **Measured neutral at the default promotion age and worse at age 1** (§3b): the split saves marking on a collector whose pause is mostly sweeping, and the sweep is O(registry) whatever the split says. `gen=` and `[GC] zgc-generational:` say whether it engaged |

**Updated 2026-08-16.** Concurrency landed. The rest of this section is kept as
written on 2026-08-13 so the diff between what was planned and what was built
stays legible; §2's per-item headers say which of C1–C5 are closed and which
are not, and C1 closed in a **different shape** than it was specified in — for
an architectural reason that is worth reading before the next collector change
proposes a background thread that takes a safepoint.

**Updated 2026-08-17.** Generational landed too, and §3 has been rewritten
around what building it actually found. The rest of this section is kept as
written on 2026-08-13 so the diff between what was planned and what was built
stays legible.

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
  **This line was wrong in a way that mattered, and §3 opens with it: it was not
  fed by anything at all.**

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
on `apps/probes/BigLive.java` (1,000,088 live objects, `-Xmx1500m`, relocation off),
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
refused throughout, `relocation_skipped_jit`), `apps/probes/ZgcConcMarkProbe.java`
at `-Xmx900m` and `apps/probes/ZgcConcMarkThreadsProbe.java` at `-Xmx1200m`.
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
  reclaims less and the next arrives sooner. That turns a per-cycle win into a
  **worse total pause**. ~~On the multi-threaded probe the total still improves
  (1.83 s → 1.27 s).~~ **Withdrawn 2026-08-17 — that figure was wrong in the
  concurrent arm's favour, because the mark-start pause was not being measured
  at all. §2c has the corrected totals: the total pause is worse on BOTH
  probes.**
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

## 2c. Why there is still a pause — the anatomy, 2026-08-17

§2b measured the pause and said nothing about what was in it. Two instruments
(`[GC] zgc-markstart:`, `[GC] zgc-pause:`, `[GC] zgc-markend:`) answer that, and
the first thing they found was a hole in §2b itself.

### The correction: the mark-start pause was measured NOWHERE

`--verbose:gc`'s `pause_us` is taken inside `collect_garbage`. A concurrent
cycle has **two** pauses, and the other one — mark start — was in no figure
anywhere. It is 22–69 ms and it happens once per cycle. Corrected totals, idle
box, same probes:

| probe | arm | collections | Σ collection | Σ mark-start | **Σ ALL PAUSE** |
|---|---|---:|---:|---:|---:|
| single-threaded | stop-the-world | 7 | 2.26 s | — | **2.26 s** |
| single-threaded | concurrent | 13 | 2.37 s | 0.47 s | **2.85 s** |
| single-threaded | concurrent (worse rep) | 13 | 3.88 s | 0.46 s | **4.34 s** |
| 8 mutator threads | stop-the-world | 4 | 1.80 s | — | **1.80 s** |
| 8 mutator threads | concurrent | 6 | 1.64 s | 0.41 s | **2.05 s** |

**Total pause is worse on both probes** — +26% to +92% single-threaded, +14%
multi-threaded. §2b's claim that the multi-threaded total improved is withdrawn.
Per-cycle it is still a win (597 → 394 µs·10³ on the threaded probe, −34%),
which is the honest statement: **concurrent marking trades total pause for
per-cycle pause**, and §2b reported only the half that flattered it.

### What the remaining pause is made of

Means over steady-state cycles (cycle 1 dropped — it collects a heap still being
built). `mark_us` is the stop-the-world marker; it is **0** on every concurrent
cycle, so the mark really did leave the pause.

| arm | total | markend | sweep | snapshot | mark | mark-start (of which clearbits) |
|---|---:|---:|---:|---:|---:|---:|
| single / STW | 375 ms | — | 139 ms (37%) | 13 ms | **224 ms (60%)** | — |
| single / conc | 197–323 ms | **80–208 ms (41–64%)** | 98–102 ms (30–52%) | 15–17 ms | 0 | 35 ms (96%) |
| threads / STW | 597 ms | — | 210 ms (35%) | 26 ms | **360 ms (60%)** | — |
| threads / conc | 325 ms | **136 ms (42%)** | 147 ms (45%) | 42 ms (13%) | 0 | 69 ms (96%) |

**Three phases, and none of them is the concurrent mark:**

1. **`markend_us`, 41–64% — the concurrent phase does not converge.**
   `scanned_at_safepoint` says **8–28% of all tracing still happens inside the
   pause**. The window (`CRATONVM_ZGC_CONC_START=60` → 40% of the threshold's
   worth of allocation) is not long enough, and the SATB replay adds tracing the
   stop-the-world arm never does. This is the biggest single lever and it is a
   constant.
2. **`sweep_us`, 30–52% — and it never leaves the pause at all.** ~100–147 ms,
   stop-the-world by construction. **This is the floor:** even with a perfectly
   converged mark, the pause on this heap cannot go below roughly
   `sweep + snapshot`. Concurrent *marking* cannot touch it; a concurrent sweep
   is a separate project.
3. **The mark-start pause, 22–69 ms, of which 94–96% is `clearbits`** — a full
   registry walk (4.6M entries single-threaded, 10.8M threaded) clearing
   `GC_FLAG_MARKED`, inside a pause, once per cycle. The stop-the-world arm
   folds the same walk into `mark_us`; the concurrent arm pays it as its own
   pause. **Fixed the same day — see the ranked list below; it is now ~0.1 ms.**
   The table above is the pre-fix state, kept because the other two rows are
   still current.

### The window sweep

One rep each, `CRATONVM_ZGC_CONC_START` swept, single-threaded probe:

| START | cycles | collection | markend | sweep | mark-start | **total/cycle** | tracing in pause | reclaimed |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 20 | 13 | 241 ms | **13 ms** | 104 ms | 22 ms | **263 ms** | **0.2%** | 29.3% |
| 40 | 13 | 291 ms | 173 ms | 101 ms | 35 ms | 325 ms | 29.6% | 27.1% |
| 60 | 13 | 194 ms | 68 ms | 109 ms | 37 ms | 231 ms | 11.6% | 27.1% |
| 80 | 9 | 399 ms | 253 ms | 133 ms | 42 ms | 441 ms | 44.0% | 39.3% |

**`START=20` converges: 0.2% of tracing in the pause and `markend_us` collapses
to 13 ms.** That is the lever working, and it confirms the diagnosis rather than
just correlating with it. The 40/60/80 rows swing non-monotonically and should
not be read individually — `markend_us` varied 80–208 ms across two reps at
START=60 alone.

**It does not fix the total, though.** Reclaim stays at 29.3% against the
stop-the-world arm's **54.3%**, so the cycle count stays doubled: 13 × 263 ms =
3.4 s of pause against 7 × 375 ms = 2.6 s. **The reclaim rate is the number that
decides the total, and the window does not move it.**

### Ranked, with what each is worth

1. **The reclaim rate, 54.3% → 27.1%.** Everything else is second order: it
   alone doubles the cycle count and it is why the total pause is worse. It is
   the floating-garbage cost of allocate-black, and Phase G (generational) is the
   structural answer. Nothing short of that has been shown to move it.
2. ~~**`clearbits`, ~95% of a 22–69 ms pause.**~~ **FIXED, same day.** The walk
   was clearing bits that were already clear: a counter reported
   `stale_marked=0` on **every one of 20 mark starts**, at two window settings.
   The sweep is exhaustive — every survivor's `GC_FLAG_MARKED` cleared, every
   corpse zeroed, the bit cleared even on the object it refuses to size — and
   objects allocated afterwards are born clear with `allocate_black_if_marking`
   inert outside a cycle. It is now behind `conc_bits_known_clear`, which
   `abandon_concurrent_mark` clears because that path drops a partial trace with
   no sweep behind it.

   | | before | after |
   |---|---:|---:|
   | mark-start pause, single-threaded (4.6M registry) | 35,441 µs | **101 µs** |
   | mark-start pause, 8 threads (10.8M registry) | 68,707 µs | **126 µs** |

   **The verification is the DEBUG test run, not the release counter.** In
   release the walk does not run, so `stale_marked` stays 0 whether or not the
   latch is honest — a vacuous zero. Debug builds do the walk anyway and
   `debug_assert` the latch; 1598 gc tests pass with that check live, and
   `the_mark_bit_walk_is_skipped_only_when_the_sweep_has_cleared_them` pins the
   abandon case specifically.

   The colour-parity alternative (`ZColor::Marked0`/`Marked1` and
   `mark_color_for`, already in `zgc::vaddr`) is no longer needed for this, and
   would be the answer only if some future path had to set mark bits without a
   sweep behind it.
3. **`sweep_us`, the floor.** A concurrent sweep, which is its own project and
   is not in Phase C.
4. **`snapshot_us`**, 13% of the threaded concurrent pause: `bases()`
   materialises a `Vec` of every registered base — 10.8M × 8 B = 87 MB allocated
   inside the pause, twice per concurrent cycle. Iterating the bitmap in place
   would remove it.

---

## 3. Phase G — generational — **BUILT 2026-08-17, opt-in**

This section said "everything here already exists as *accounting*; what is
missing is a cycle that uses it". Building the cycle found why nothing had ever
used it, and the answer was not that the cycle was missing.

### G0 — the card barrier was on no store path at all

`note_ref_store` was reachable only from `GarbageCollector::write_barrier`, and
this backend's `set_field` never calls it. The interpreter says otherwise in as
many words —

> write_barrier fires automatically inside set_field / set_field_volatile

(`vm/src/runtime/interpreter/opcodes.rs`) — and that is **true of `gen_heap`**,
whose `set_field` does call it, and **false of ZGC**. So every interpreted
`putfield` and every `aastore` skipped the card barrier. `remembered_roots`
having no non-test caller was not an unfinished phase; it was a hole, and the
three tests covering the barrier all passed because each called
`note_ref_store` by hand.

This is the shape this tree keeps finding: **an inert registration looks exactly
like a missing feature.** The barrier now sits on the store **accessors** —
`set_field_no_satb` (which both `set_field` and `set_field_suppress_satb` funnel
through) and `set_array_element` — for the same reason the SATB barrier does:
coverage becomes a property of the *one* store path instead of a property of a
call-site census that has to stay complete forever. `System.arraycopy` is the
case that makes call sites hopeless; it copies a reference array one element at
a time straight through the accessor.

### G1 — a young-only collection — **CLOSED**

**Not a second `collect_garbage`.** That was the obvious shape and it is the
wrong one here: `collect_garbage` also runs a finalizer resurrection pass, the
reference processor, a soft-referent remark, two censuses and a sweep that
coalesces the arena, and a second copy of all of it would be a second place for
the old code and the new to disagree about what "live" means.

Instead a young cycle **inverts one arm of a pass that already runs**. The
mark-bit clear SETS the bit on old objects, and every phase downstream is then
correct untouched:

| phase | why it needs no change |
|---|---|
| the mark loop | its `already visited` test skips old objects, so their fields are never enumerated — **this is the entire saving** |
| `process_references` | asks `is_marked_addr`, so an old referent reads as live and is not cleared |
| the finalizer pass | sees an old finalizable object as a survivor, so it waits for a major rather than being finalized early |
| the sweep | sees old objects as survivors, clears the bit, retains them |

What had to be written is only what that inversion cannot express: the roots the
old generation contributes, the aging and promotion the sweep performs, and the
card the promotion has to leave behind.

#### The split is per OBJECT, and the page grid cannot do it

`page_ages` + `age_pages_and_split` were the obvious input and they are the
wrong one. A page's age only rises; the bump cursor sits inside a page that has
usually already aged past the promotion age; and in steady state the allocator
serves most requests out of free-list holes scattered over every page. So a
freshly allocated object — **precisely what a young cycle exists to collect** —
would be born into an old page, read as old, and the phase would reclaim nothing
while still paying for the barrier.

`ObjectHeader::gc_age` has none of that: it is 0 at allocation wherever the bytes
came from, and it is the field `gen_heap` and G1 already promote on. The
page-keyed remembered set stays, because a card table is what it is — a page id
is an index, not a claim about the generation of everything on that page.

`page_ages` / `age_pages_and_split` / `old_page_ids` are unchanged and still feed
`ZGenerationScope` from inside `relocate_stw`. They are page-level *accounting*
now, and nothing gates on them.

#### The three correctness rules, each asserted in both directions

1. **A card is cleaned when its object no longer points into young, and kept
   while it does.** Never clearing is correct and unbounded — the root scan
   converges on re-enumerating the whole old generation and the phase saves
   nothing. Always clearing is a use-after-free: the edge outlives the store that
   created it, and nothing rewrites the field to re-card it. `swap_all` +
   re-dirty is why `ZRememberedSet` has two buffers.
2. **Promotion cards the object it promotes.** The barrier cards a store only
   when the receiver is *already* old, so every reference an object wrote during
   its young life is un-carded at the instant it is promoted. This is the
   per-object answer to the hazard this section originally stated in page terms
   ("the first young cycle after a promotion must treat the newly-old page as
   wholly dirty") — and per object it is **exact**, costing one card instead of a
   scan of every object on the page.
3. **The four pin registries are rooted wholesale.** A young cycle never
   *visits* an old object, so it never pushes the edges `collect_garbage`'s mark
   loop pushes per marked object: the class's loader, that loader's mirrors, its
   metadata roots, and the native collection overlays it owns. **Not one of the
   four is written through `set_field`**, so no card could ever have covered
   them. An old `HashMap` with a native overlay holding young contents had those
   contents freed while the map was live — no wild pointer, no failing
   assertion, and the symptom is a collection that has silently emptied.

   Rooted wholesale rather than per old object: O(registry) once instead of
   O(old objects) × four global lookups, and safe in the right direction.
   `gen_heap` already does this for the overlays, which is why the predicate form
   of the provider API exists.

   **The test that catches it was red before the fix, and the ZGC fixture's own
   `roots_for_matching_owners: |_p| Vec::new()` stub was hiding it.** The real
   provider in `native-collections` implements that arm, so the stub was the
   difference between the fixture and production, not the feature.

#### Two interactions that are refusals, not features

* **A concurrently-marked cycle is never a young cycle.** The mark set
  `finish_concurrent_mark` hands over *is* the whole-heap closure; it cannot be
  scoped after the fact, and re-marking would throw away the concurrent phase's
  whole product. Same for a collection driven by allocation failure: a young
  cycle retains the entire old generation unexamined, so it is the wrong tool
  for "the heap is full".
* **A relocation that moved anything re-cards the old generation.** A card is a
  page id plus an offset, so a slide invalidates every one at once; object ages
  ride in the header and are unaffected, which is what would have made the
  failure silent. Rebuilt from the post-slide live set rather than rewritten
  through the pointer map, because a card whose object did not move has no map
  entry and neither does one whose object died — and the difference between those
  two decides between a lost edge and a leak.

**Exit criterion**, as written: *a young cycle collects a young-only workload
with strictly less work than a full cycle, and a test proves an object reachable
only through an old field survives it.* Both are met —
`a_young_cycle_keeps_an_object_reachable_only_through_an_old_field` drives the
store **accessor** and then a real collection, which is the only version of that
test that could have failed before G0; and `old_retained` is the count of objects
a young cycle retained without tracing. §3b has the measurement.

#### What Phase G costs, stated

* **The card barrier on every reference store.** One relaxed load and a
  not-taken branch until the first promotion; after that, for a store whose
  receiver is old, one header read plus a bitmap `fetch_or` under a per-page
  lock. Nothing on the read path.
* **The remembered set's memory.** A card is one bit per 8 bytes over a 2 MiB
  logical page, double-buffered: **64 KiB per page that has ever taken a store
  into an old object**. On a 1.2 GB heap that is up to ~38 MB, or 3%.
  `young_extra_roots` drops a set that has become empty, so the steady state is
  the pages that really hold old objects with live young references — but a run
  that never takes a *young* cycle never cleans, so the ceiling is reachable.
* **Floating garbage in old.** A young cycle retains every old object without
  asking whether it is reachable, so garbage promoted before it died is
  invisible until the next major. That is what the `minors_per_major` ceiling
  bounds and what `zgc_gen_minors_per_major`'s note is about; it is the phase's
  defining trade, not a defect.
* **A whole-heap sweep on every cycle, including young ones.** See G2.

### G2a — a nursery FLOOR, so a young sweep is O(young) — **BUILT 2026-08-17**

§3b's finding was that the split works and the pause does not move, because
`sweep` was 182 ms of a 309 ms mean and **walks every registered object whatever
the split says**. The split decides how much a young cycle *traces*; it says
nothing about how much it *sweeps*.

**A sweep can only be bounded by address.** The registry is a bitmap over the
arena indexed from its base, so a lower bound on the address is a lower bound on
the word index — the scan simply starts later. Everything allocated since the
last whole-heap collection lies at or above that collection's final cursor, so
`[gen_young_floor, cursor)` is a nursery in the ordinary bump-allocator sense and
a young cycle sweeps only that. A whole-heap cycle publishes the floor and the
old live-byte total; a young cycle sweeps above the floor and adds the total back.

Four things have to hold together, and each alone is satisfiable by something
broken:

| must hold | what it is satisfiable by otherwise |
|---|---|
| the sweep really skips | a floor stuck at 0 makes `for_each_base_from(0, ..)` the unbounded loop and **every test still passes** — that is the state §3b measured. `gen_sweep_skipped` is the engagement counter, and the test was verified by disabling the floor and watching it go red |
| young garbage above the floor is still reclaimed | a sweep that skips everything is fast and useless |
| `allocated` still reports the WHOLE live set | the sweep counts only what it visited, so uncorrected the heap reads as nearly empty after every young cycle and the trigger stops firing until an allocation fails. `gen_old_live_bytes` is carried forward; `objects_copied` is deliberately NOT corrected, because it means "survivors this cycle examined" |
| `conc_bits_known_clear` goes FALSE | objects below the floor keep the mark bit the pre-mark pass set, so the next mark start must not skip its clearing walk — that would hand the sweep a mark set carrying a previous cycle's bits |

**The cost, and it is asserted rather than assumed.** The free list hands out
space *below* the floor, so an object allocated into a hole left by an earlier
sweep is inside the old region and a young cycle will not reclaim it until a
major. That is over-retention, never unsoundness — the test pins **both** halves,
that it survives the minor and that the major gets it, which is the difference
between a bounded cost and a leak. Removing it is what needs a real young space,
i.e. G2 below.

A slide drops the floor and arms `gen_force_major_next`: after a relocation an
address no longer says which generation an object is in, and re-deriving the
boundary would be guesswork. A floor left *above* the cursor by a tail retraction
recovers the same way, and there is a test for it — otherwise it stalls collection
until an allocation fails, which presents as a slow leak rather than as a bug.

#### It is NOT yet measured against §3b, and here is why not

The re-run was attempted on 2026-08-17 and the host was at **load 25–35 on 8
cores** for its whole duration (other sessions building and running suites).
§3b's table was taken on an idle box, so the two are not comparable and no pause
figure from that run is quoted here. This tree has a standing rule about exactly
this: a shared host's load invalidates a timing comparison, and one
un-interleaved run in §2b already said "free" and was noise.

**What IS load-independent, and what to check first on the re-run:** the
engagement counters. `swept=A/B` on each `[GC] zgc-real:` line and
`sweep_skipped` on `[GC] zgc-nursery:` are counts, not timings. `A == B`, or
`sweep_skipped=0` with `young_cycles>0`, means the floor never moved and the whole
change is inert — and that is the state §3b was in for the split itself, so it is
the first thing to read, before any pause number.

The comparison to make, once the box is quiet: the same probe and arguments as
§3b (800k retained, 600 rounds of 30k churn, `-Xmx1200m`, arms interleaved), and
the number to watch is `sweep` in `[GC] zgc-pause:` — 182 ms of a 309 ms mean
before, and O(young) is only worth having if that falls.

### G2b — the slide IS the promotion — **BUILT 2026-08-17**

With the JIT-load-barrier blocker gone (see the correction under the five-step
list above), the promotion increment turned out to be a **deletion plus two
stores**, because the machinery was already there.

`compact_low_to` packs survivors from the first selected page upward and drops
the cursor to the end of the compacted region, so after a slide **every live
object is below the cursor** — the ones on unselected dense pages never moved and
are below it too. G2a's floor was being *thrown away* at that point, on the
reasoning that a slide rewrites the low region so an address no longer says which
generation an object is in. That was backwards: a slide rewrites the low region
into exactly the shape a nursery wants.

So the floor is now re-established at the post-slide cursor. Three consequences:

* **it is promotion by copy** — survivors are moved out of the young region and
  the nursery is left EMPTY, which is G2's defining behaviour;
* **it carries no floating garbage**, because the registry at that point holds
  live objects only (the sweep pruned the dead a few statements earlier), so
  declaring everything below the cursor old retains nothing unreachable. That is
  strictly better than G2a's floor, which inherited whatever the free list had
  placed below it;
* **a relocating cycle no longer forces a whole-heap cycle behind it.** It used
  to arm `gen_force_major_next`, which meant the split was off every other
  collection whenever relocation was on — i.e. by default.

`gen_promotions_by_slide` is the engagement counter: `compaction_cycles > 0`
with that at zero is the old behaviour exactly. The test asserts the counter, that
**no** live base sits at or above the floor, and that the force-major latch stays
clear — and it was checked for the vacuous case, that the selector really does
move in its fixture rather than declining and passing through the early return.

### G2c — the nursery is where allocation GOES — **BUILT 2026-08-17**

G2a had to document a cost: `Arena::alloc` serves the free list before the bump
cursor, so an object placed in a hole below the floor is in the old region and no
young cycle reclaims it — it waits for a major.

That default is right for a non-generational heap and the comment in `alloc` says
why: after a sweep that could not move survivors, hole reuse is the only thing
keeping the arena from ratcheting. It is wrong for a nursery, so ZGC now asks the
arena for **bump-first** (`Arena::set_prefer_bump`) whenever generational mode is
on, and for nobody else.

**It cannot cause an `OutOfMemoryError`, and that is the property that made it
shippable.** It skips only the free-list *fast* path; `alloc`'s post-bump retry
searches both tiers in full and then coalesces and searches again, so once the
bump tail is exhausted every hole in the arena is still reachable. Turning it on
can change *which space* serves an allocation, never *whether* one succeeds — and
on a non-compacting heap the layout policy is the OOM policy, so that had to be
argued rather than assumed. `Arena::free_list_after_bump` counts the
fall-throughs, so "the nursery is leaking into the old region because the bump
tail is exhausted" is a number rather than an inference.

The holes below the floor are recovered by the compacting slide — which is
default-on, and is also what promotes the nursery's survivors out (G2b). The three
pieces close on each other: **G2a** bounds the sweep, **G2b** empties the nursery,
**G2c** fills it.

Both directions are tested, because the ON assertion alone would be satisfied by
an allocator that happened not to reuse holes: the same fixture with the mode OFF
must put objects back in the swept holes.

### G2d — a BOUNDED nursery, so young cycles actually happen — **BUILT 2026-08-17**

`needs_gc`'s two clauses are both about the **whole heap**: live bytes against
`gc_threshold`, and allocatable space via `headroom_low`. Neither ever asks
*"has enough been allocated since the last collection to be worth a young
cycle?"* — so a young cycle happened only when a full collection would have. Young
cycles were exactly as rare as the collections they were meant to replace, and
§3b's six-in-600-rounds is that, not a property of the workload. **A split that
runs six times cannot pay for a barrier on every store.**

`CRATONVM_ZGC_GEN_NURSERY_PERCENT` (default 10, `0` is the kill switch) adds the
missing clause: `allocated - watermark >= budget`, where the watermark is
`allocated` as of the end of the last collection. It needs no counter of its own —
`allocated` is already incremented on the allocation path and already loaded by
`needs_gc` — so a miss costs two relaxed loads and two compares.

**It deliberately bypasses `gc_rearm`, and that is why the anti-storm property
had to be argued.** `gc_rearm` is a quarter of remaining headroom, which is far
larger than a nursery budget, so ANDing them would make the clause unreachable.
It does not need the floor: the watermark is reset by *every* collection, so
firing again requires a fresh budget's worth of genuinely new allocation. A live
set parked above the threshold cannot re-trigger it, which is the one thing
`gc_rearm` exists to prevent — and the test pins exactly that, asserting the
trigger is disarmed immediately after the collection it asked for.

**10%, not `Z_DEFAULT_YOUNG_FRACTION`'s 25%.** That constant describes a young
generation's share of a heap it *owns* — a sized space survivors are evacuated out
of. This nursery is the tail of one arena, reclaimed by a bounded sweep rather
than a cursor reset, so its cost is proportional to the objects in it. A smaller
budget is what buys frequent cheap cycles; 25% of a 1.2 GB heap gives six large
ones, which is what §3b already measured. Revisit when a young cycle reclaims by
resetting a cursor.

`gen_nursery_triggers` is the engagement counter, and it is a **latch consumed by
the collection**, not a `fetch_add` in `needs_gc` — the predicate is polled on the
allocation path and stays true from the moment it is reached until the collection
runs, so counting observations would report allocations rather than collections.

### G2e/G2f — what the sweep does to each dead object — **BUILT 2026-08-17**

G2a bounded *which* objects a young sweep visits. §3b then measured the phase
engaged, correct, and not paying: with 4.8M objects skipped per young cycle the
pause did not move. These two bound what the sweep *does to each object it does
visit*, and both terms it removes are **O(reclaimed volume)** rather than
O(objects) — which is why bounding the walk could not move the number.

**G2e — zero the HEADER, not the body.** The sweep's own comment gives the reason
for zeroing: "so a later scan can't see a stale header". That reason is satisfied
entirely by the header. `HEADER_SIZE` is the whole `ObjectHeader` (`class_id`,
`shape`, `mark_word` — 4 + 4 + 8) and `ARRAY_DATA_OFFSET == HEADER_SIZE`, so an
array's length lives in `shape` and not in a body prefix; zeroing those 16 bytes
leaves the identical `class_id=0, num_slots=0` corpse a reader of a vacated span
already met (see `corpse_ledger`, which exists *because* that is what a reader
sees). And the body is only reachable **through** that header: every field read
sizes the object from `num_slots`, every extent walk from `alloc_size(header)`,
every membership test goes through the registry the sweep has just removed the
base from. Nothing can reach the bytes this stops writing.

The other candidate reason — "a reused block may contain stale bytes" — is
already handled at the far end, and unconditionally: `alloc_raw` memsets every
allocation it hands out, and `tlab_refill` memsets a whole chunk. So the body
zeroing was **redundant with the allocator's**, and every dead object was memset
twice: once when it died, once when its span was handed out again. What it cost
was a memset of the entire reclaimed volume, inside the pause, every cycle — on a
cycle that reclaims 900 MB, 900 MB of zeroes at memory bandwidth.

**G2f — one free-list span per RUN of adjacent dead objects.** The walk is
already ascending, and that is a correctness property rather than an accident:
the coalescer only sees adjacent dead spans as adjacent because they arrive in
order. So the merge is a comparison against the previous span's end, and objects
die in runs. `coalesce_free_list` merges exactly these spans into exactly these
maximal runs a few statements later, so the post-coalesce state is the same
either way — which is **asserted** in
`an_arena_coalesces_pre_merged_runs_to_the_same_shape` and not assumed, because
the tier a span routes to genuinely differs on the way in (a merged run is large
where its members were small) and "the coalescer normalises it" is the whole
safety argument. What goes is the churn: 4.8M `push_block_routed` calls and a
4.8M-element sort inside the pause.

Three ways the merge can be silently wrong, all guarded:

* **Crossing into the large-object end.** `add_free_block` routes by offset and
  bounds a low span by the low cursor, so a run grown past `high_cursor` would be
  pushed onto the low tier while covering high-region bytes — and the arena would
  then serve the same memory from the free list and from the high cursor both.
  The two ends share one middle, so this is reachable, not hypothetical.
* **Handing a span over out of order.** The non-mergeable arm must flush the
  pending run first, or the coalescer stops seeing adjacency for the rest of the
  cycle. That is the measured `CopyChurn`-at-`-Xmx256m` failure (§ G2a), not a
  tuning matter.
* **The final run.** It has no successor to flush it, and if it is dropped the
  span is simply never mentioned again — no assertion in the collector fires and
  the cursor retraction cannot reclaim what is not on the list. So the test
  asserts the *accounting identity* (every freed byte is on the free list or
  below a retracted cursor) rather than a counter.

Both are ANDed with `young_cycle`, so a whole-heap sweep is byte-for-byte what it
was. That is not a claim the argument is weaker for a major — it is not — only
that this landed during a gauntlet sweep and the arm under measurement is the one
already behind `CRATONVM_ZGC_GENERATIONAL`. Promoting either is one condition,
with its own measurement.

Kill switches in the `zgc-relocate` shape, so the A/B is a re-run and not a
rebuild: `CRATONVM_ZGC_GEN_HEADER_ZERO=0` restores the whole-body memset,
`CRATONVM_ZGC_GEN_DEAD_RUNS=0` the per-object calls. Engagement counters on the
`[GC] zgc-sweep-cost:` line, read against `young_cycles`:
`zero_bytes_skipped=0` means every dead object was still memset in full, and
`dead_runs == dead_objects` means no two dead objects were ever adjacent. Neither
number says it alone — a small `dead_runs` is equally consistent with a cycle that
found almost no garbage — which is why both are reported.

**MEASURED 2026-08-17 — see §3d. One of the two pays, the other measures zero,
and the prediction written here before the run was wrong.**

That prediction was: `bytes_freed` on the `[GC] zgc-reclaim:` line is the memset
volume G2e removes, "of the order of 900 MB per cycle against a 182 ms sweep".
Both halves were the wrong number for the wrong cycle. The measured memset volume
is **400 MB across 24 young cycles — 17 MB each**, because **G2d had already
capped it**: a bounded nursery bounds the garbage a young cycle reclaims, so the
earlier change took most of the win the later one was predicted to. And 182 ms was
a mean over *all* cycles, dominated by the whole-heap ones; a young cycle's sweep
was 9.0 ms before this and 6.3 ms after.

Reading `bytes_freed` off a whole-heap cycle and calling it a young cycle's memset
volume is the same category error §3b made about `sweep` in the first place.

### G2 — what is still missing, and it is smaller than it was

With G2a/b/c in, the young generation is a real address range that allocation
fills, a bounded sweep reclaims, and a slide promotes out of. What a
`ZPageAllocator`-based young space would still add:

* **Reclaim with no sweep at all.** A young cycle still walks the nursery's
  registered objects. A page-based young space frees a whole page by resetting a
  cursor, so the survivors' cost is the copy and the garbage costs nothing.
* ~~**A bounded nursery.**~~ **Done by G2d above.** The nursery now has a size
  budget that triggers a collection of its own, so its cost no longer scales with
  the gap between whole-heap collections. What a *sized space* would still add on
  top is a hard ceiling rather than a trigger — today an allocation burst can
  overshoot the budget before the next safepoint. **Priced 2026-08-17, and it is
  not the collector's to fix.** `nursery_overshoot_max` on the
  `[GC] zgc-nursery-trigger:` line reports the worst overshoot seen, beside the
  budget it is an overshoot of. A real ceiling cannot live where the trigger
  lives: refusing the allocation turns a servable request into an
  `OutOfMemoryError`, and collecting on the spot needs a safepoint the allocation
  path cannot take. So a ceiling needs an **allocation-site safepoint poll**,
  which is a VM-wide change, and the gauge is what says whether it is worth
  asking for — a few percent of the budget prices it at nothing.
* **Per-page `ZObjectStarts`**, without which `is_object_address` stops being
  O(1) once the registry is per page.

None of it is blocked on the JIT load barrier (stage (a) landed 2026-08-13), and
none of it is needed for correctness — it is throughput and predictability. Treat
the allocator swap as its own change with its own measurement, as this section has
said from the start.

This item said promotion needs `ZPageAllocator` because "a logical grid cannot
separate young from old in address space". That is true and it stopped being the
blocker it was described as twice over: **G1 does not separate them in address
space at all** — it separates them by header age — and **G2a/b/c now do separate
them in address space**, using the arena's own cursor rather than a page
allocator.

What is still missing, and what it would buy:

* **Moving survivors into an old space.** Today a promoted object stays where it
  is. A real young space would give the young generation contiguous, sequentially
  allocated memory and let a young cycle reclaim it by *resetting a cursor*
  rather than by sweeping the whole registry. That is the answer to
  `sweep_us` — 30–52% of the concurrent pause and the pause floor (§2c) — and it
  is the only one, because a sweep over a flat registry is O(all objects)
  whatever the generation split says.
* ~~**A young-only sweep.**~~ **Done by G2a above, partially.** The sweep is now
  bounded below by the nursery floor, so it is O(young) for everything the bump
  cursor served. What a real young space adds is the *other* half: an object the
  free list placed below the floor is still swept only by a major, and a page-based
  young space has no free list below anything — it reclaims by resetting a cursor,
  so there is no sweep at all.

Those two are the same project and it does need the page allocator. Sequence it
after C5, and treat the allocator swap as its own change with its own
measurement.

**What the swap concretely involves**, so the next attempt starts from a list
rather than from "replace `Arena`":

1. `alloc_object` / `alloc_array` allocate from a **young** page rather than the
   arena's bump cursor, and the large-object path from its own size class.
2. `zgc::tlab` carves chunks out of young pages instead of the arena, and
   `retire_all_tlabs` returns them to pages.
3. Promotion **copies** a survivor into an old page. That is the first time this
   collector moves an object outside `relocate_stw`, so it needs the same
   root-rewriting and pointer-map plumbing — and it needs the JIT load barrier,
   for the reason `zgc_relocation_permitted` already refuses relocation whenever
   the JIT is on.
4. A young sweep frees whole young pages by **resetting a cursor**, which is the
   part that answers `sweep_us`. Objects that survive are gone from the page by
   step 3, so there is nothing to walk.
5. `ZObjectStarts` becomes per page rather than one flat bitmap over the arena,
   or `is_object_address` stops being O(1).

Steps 1, 2 and 5 are the allocator swap; 3 and 4 are the generational part and
cannot be done first.

**CORRECTED AGAIN 2026-08-17: what step 3 IS gated on is the moving-young
coverage proof, and ZGC consults it zero times.**

Saying step 3 is not gated on the load barrier was right and it was not the whole
answer, because it left the impression that nothing gates it. Something does, and
naming it is the difference between a five-step list and a plan.

`relocate_stw` refuses outright whenever `gc_quiescence::is_active()` or
`unregistered_jit_frame_on_stack()` — a compiled frame may hold object pointers
in **registers and spill slots**, which are not slots the collector can find and
not slots it can rewrite. Promotion by copy is a move, so it inherits that
refusal exactly. Measured 2026-08-15 on a deliberately JIT-saturated workload:
the refusal fires on **64 of 68 cycles**. A young space whose reclaim is "copy the
survivors out and reset a cursor" therefore would not run at all on a JIT-hot
workload — it would divert to the very sweep it was built to remove.

**This VM already solved that problem, on the other collector.** `gen_heap`'s
moving young generation is exactly a promote-by-copy cycle running under live
compiled frames, and it gets there with a **coverage proof** rather than a
barrier: `CRATONVM_MOVING_YOUNG`, per-safepoint oop maps and shadow homes
published from the JIT entry chain, plus `moving_young_coverage_incomplete()` and
`force_non_moving_jit_roots()` as the fail-closed conditions
(`gen_heap.rs:5730`ff, and read the deleted second gate there — a flag whose only
job was to permit correct behaviour). That machinery makes a compiled frame's
roots *precise and rewritable*, which is the property a move needs and a load
barrier does not supply: a barrier fires on a load, and a pointer already sitting
in a register is never loaded again.

**ZGC consults none of it.** `grep moving_young gc/src/zgc.rs` returns zero hits.
So the honest critical path for steps 3 and 4 is *adopting the moving-young
coverage proof onto this collector*, and it should be sequenced and priced as
that rather than as an allocator swap with a promotion step attached.

Which is also the argument for G2e/G2f above: they reduce the same `sweep_us` and
they run on **every** cycle, JIT-hot or not, because they move nothing.

**On the JIT load barrier, for the record: step 3 is NOT gated on it.** This
paragraph said its dependency on that barrier was "the real critical path", and
the source says otherwise: **stage (a) of
[`zgc-jit-load-barrier.md`](zgc-jit-load-barrier.md) landed on 2026-08-13.**
`zgc_codegen_honours_read_barrier()` returns `true`,
`zgc_read_barrier_blocks_inline_fields()` routes every compact-field access
through the barriered helpers when a cycle arms the barrier, and
`zgc_relocation_permitted` therefore permits relocation with the JIT enabled.
Relocation has been moving objects under the JIT since then — the 2026-08-17
Phase G run recorded it.

And promotion needs no more than relocation does: it happens at a **safepoint**
and the collector rewrites every slot itself, so it inherits both of
`relocate_stw`'s protections (the permitted-configuration gate, and its own
refusal while a JIT frame is on a stack). A load barrier is what **concurrent**
relocation needs. The lesson is the tree's own: re-derive a stated blocker from
the source before pricing work around it — this one had been closed for four
days.

## 3b. What Phase G actually measured — 2026-08-17

`apps/probes/ZgcGenProbe.java`: 800,000 retained linked nodes (~134 MB live), 600
rounds of 30,000 short-lived nodes each, and an old-to-young store into every
16th retained node per round — 48M objects allocated, `-Xmx1200m`, idle Azure
box, three arms **interleaved**, two reps.

`off` is generational off; `g3` is on at the default promotion age 3; `g1` is on
at promotion age 1.

| arm | cycles | Σ pause | reclaim | `old_retained` | wall (r1 / r2) |
|---|---:|---:|---:|---:|---:|
| off | 9 | 2.78 / 2.89 s | 75.7% | 0 | 115.6 / 119.8 s |
| **g3** | 9 | **2.85 / 3.00 s** | 75.7% | **4.8M** | 115.6 / 123.2 s |
| **g1** | 11 | **4.06 / 4.49 s** | **63.7%** | **10.4M** | 119.8 / 127.6 s |

### It is engaged, and it is correct

`young_cycles=6`, `gen=young/800087` on cycles 4–8, `old_retained` 4.8M,
`remembered_roots` 4.8M. Every arm returned `BAD=0` **and**
`written_intact=800000` — that second number is the one that matters: it is the
count of objects reachable *only* through an old-generation field that were still
intact at the end, so every one of 800,000 old-to-young edges survived six young
cycles. That is G1's exit criterion, end to end, on a real workload.

### It does not pay on this workload, and the reason is measurable

At the default promotion age the total pause is **+2.5% to +4%** and the reclaim
rate is identical; at promotion age 1 it is **+46% to +55%** with reclaim down
from 75.7% to 63.7% and two extra cycles. Wall clock is flat within noise
throughout.

The per-cycle lines say why. A young cycle's pause is 280–300 µs·10³ and a major
cycle's is 281–320 — **indistinguishable** — while `sweep` is 182 ms of a 309 ms
mean pause. The mark was never the bottleneck here: a young cycle skipped tracing
800k of ~1.7M registered objects and the pause did not move, because **the sweep
walks every registered object whatever the generation split says.** That is not a
surprise, it is what §3's G2 predicted in as many words; the measurement is what
turns it from an expectation into a number.

`g1` is worse for a second, separate reason: promotion age 1 promotes everything
that survives one cycle, including churn that happened to survive — 11.7M
promotions — so the old generation fills with floating garbage no young cycle
will examine, reclaim falls, and the cycle count rises. **A lower promotion age
is not a stronger version of the same knob.**

### So G2 is not an optimisation of Phase G; it is what makes Phase G worth having

A real young space is reclaimed by *resetting a cursor*, which is the only thing
that removes an O(registry) sweep. Until then the generation split saves marking
on a collector whose pause is mostly sweeping. **`CRATONVM_ZGC_GENERATIONAL`
stays off by default, and now for a measured reason rather than out of caution.**

### The first attempt at this measurement was vacuous, and that is the second time

The 2026-08-17 run before this one came back `off 62.8 s → g3 55.9 s → g1 45.7 s`
— an apparent **−27% wall-clock win** — with `young_cycles=0` on *every arm*, the
flag on and 3.2M promotions logged. Every one of those differences was noise. The
cause was the trigger forcing a major whenever `headroom_low` was set, on a heap
where the live-bytes threshold is never reached and so *every* collection is
allocation-driven (see `gen_force_major_next`).

Only the engagement counter separated that from a result. §2b's withdrawn
multi-threaded pause claim was the same lesson one section earlier: **print what
the arm CHANGED beside what it COST, or a comparison of two arms that both did
the same thing reads as a finding.**

---

## 3c. C5 re-measured — the three locks were not the bottleneck

`apps/probes/BigLive.java` at width 4000 / depth 250 (~1M live nodes), `-Xmx1500m`,
relocation off, `CRATONVM_ZGC_PARMARK` swept, interleaved, two reps. Mean pause
per collection; `mark_kinds` was `stw-parallel:7` on every non-zero arm and
`stw-serial:7` at zero, so every arm did what its name says.

| workers | mean pause r1 | r2 | vs 0 | mean `mark_us` r1 / r2 |
|---:|---:|---:|---:|---:|
| **0** | **103.9 ms** | **98.7 ms** | — | 86.9 / 82.9 |
| 1 | 205.6 | 285.2 | **+98% / +189%** | 176.2 / 255.0 |
| 2 | 212.4 | 265.1 | +104% / +169% | 184.0 / 181.4 |
| 4 | 638.7 | 380.1 | +515% / +285% | 521.5 / 342.4 |
| 8 | 364.4 | 466.8 | +251% / +373% | 346.4 / 431.4 |

**The exit criterion is still unmet and the diagnosis was wrong.** All three
per-object locks are gone (§4's item 1) and four workers still lose to zero by
3–6×. More decisively: **one worker is +98% to +189%**, and a single worker
contends with nobody. Lock contention cannot explain a cost that is already
doubled at one worker.

So the 2026-08-14 note's "a fixed ~30% for driving at all, and contention on
top" had the split backwards: the **fixed cost is the problem** and it is far
larger than 30%. Candidates, none of them yet measured: the per-cycle pool
construction and driver-thread spawn; the striped queues replacing a plain `Vec`;
and `ZMarkContext::visit_refs` being intrinsically more expensive than
`enumerate_references` (it is a fork of it, for stated reasons, and the fork has
never been priced).

### One part of that fixed cost was a wait nobody notified — FIXED 2026-08-17

Found by reading rather than by profiling, and it is worth recording *how*,
because the profile would not have shown it as anything but "slower".

**The mark driver's fixed-point wait was the one wait in the marker that is not
notified.** `ZgcConcurrentMarkController::await_fixed_point` polled
`ZgcConcurrentMarkState`'s *own* condvar on a `DRIVER_POLL_MS` = 5 ms grid. The
only thing that ever notified that condvar outside a stop was
`notify_work_available`, and it had **zero callers on any ZGC path** —
`vm_heap.rs`'s single call site is G1's controller. So every pass of every cycle
waited out the full 5 ms before noticing a fixed point the workers had often
reached immediately. Every *other* wait in `mark.rs` is properly notified
(`worker_idle` sets `terminated` and calls `notify_all`; publishing work bumps
`work_generation` and notifies; `release_pause` notifies) — this one was not, and
it was the one the pause waits on.

**Why it survived.** Every wait in the marker is a `wait_for` and never a bare
`wait`, deliberately, so a lost notification costs a poll interval instead of a
hang. That insurance makes the failure *invisible*: a wait that always times out
behaves identically to one that is notified, only 5 ms slower, and no assertion
anywhere fires. `ZMarkTerminator::park_timeouts` and `park_termination_wakes` now
count expiries and termination-edge wakes, and `[GC] zgc-mark-wait:` reports the
first at shutdown — **counts, so they read the same on a loaded host as on a
quiet one**, which is the whole reason they are counts.

**And the stated reason for polling was wrong about the API it described.** The
`DRIVER_POLL_MS` comment said `ZMarkTerminator::wait_for_fixed_point` "can only
be released by the *pool's* stop flag, not by this controller's". It takes the
stop flag **as a parameter**; it is `ZMarkCoordinator`'s no-argument *wrapper*
that hardwires the pool's flag, and the comment described the wrapper while the
driver had the terminator in hand. A test's doc had inherited the same claim —
"if it waited on the pool's own condvar instead, this would deadlock, which is the
whole reason `await_fixed_point` polls" — and that test
(`stopping_the_driver_mid_cycle_does_not_hang`, run against a deliberately wedged
pool) now passes with the driver waiting on exactly that condvar. Interruption is
in fact *faster* than before: the flag is re-read every `Z_MARK_PARK_POLL_MS`
**and** kicked directly by `ZMarkTerminator::wake_blocked_waiters`.

**How much of C5 this is: a small part, and it must not be reported as more.** At
`Z_PARMARK_RESTART_BUDGET = 1` the driver waits at most twice per cycle, so the
ceiling is ~5–10 ms of a ~100 ms gap. What it removes is a **floor** under the
parallel-mark pause that no amount of worker scaling could have reached, and a
5 ms quantum inside the loop that every per-cycle timing above was carrying as
instrument noise. Fix the instrument, then measure — the remaining candidates in
the paragraph above are unchanged and still want `perf record` on a one-worker
cycle.

**The next step for C5 is `perf record` on a one-worker cycle, not another
counter.** That is this tree's own standing rule and three rounds of lock hunting
against a fixed cost is what ignoring it looks like. Note also the spread — 380
to 639 ms at four workers — so any future comparison needs more than two reps.

The lock removals are kept regardless: they are correctness-neutral, they help
the serial marker too, and one of them
(`metadata_pin::roots_for_loader` cloning a `Vec` under an `RwLock` per marked
object) was a cost its own module had already documented and written a fix for
that nothing called.

---

## 3d. G2e/G2f measured, and §3b's diagnosis corrected — 2026-08-17

`apps/probes/ZgcGenProbe.java` at `800000 30000 600` (800k retained, 600 rounds of 30k
churn), `-Xmx1200m`, `CRATONVM_ZGC_GENERATIONAL=1`, defaults otherwise. One
binary, four env combinations, **arms interleaved**, two reps — so any drift from
the neighbour benchmark on the host hits every arm equally. Host at load 1.8–3.2
on 8 cores (two cores taken by another session), against the load 25–35 that
invalidated the first G2 attempt.

Means over the **24 young cycles** and the **7 whole-heap cycles separately**. A
mean over all 31 is worthless here: one whole-heap cycle sweeps 13.0M dead objects
and a young cycle sweeps 137k, so the "mean" is the major.

| arm | `HEADER_ZERO` | `DEAD_RUNS` | young `sweep_us` | young `mark_us` | young `total_us` | full `sweep_us` |
|---|---|---|---:|---:|---:|---:|
| **A** | on | on | **6381 / 6242** | 105132 / 100770 | 114215 / 109719 | 170128 / 168733 |
| **B** | off | off | **9223 / 8821** | 101545 / 100090 | 113482 / 111569 | 168900 / 165842 |
| **C** | off | on | **6331 / 6308** | 101586 / 101439 | 110781 / 110497 | 172216 / 170915 |
| **D** | on | off | **9179 / 8913** | 101031 / 100412 | 112892 / 112035 | 169114 / 166059 |

### The attribution is unambiguous, and it is corroborated twice

**A ≈ C** (6312 vs 6320 — 0.1% apart) and **B ≈ D** (9022 vs 9046 — 0.3%). Both
pairs differ only in `HEADER_ZERO`, so **G2e is worth nothing measurable**, said
from two directions rather than one. Grouped by `DEAD_RUNS`, (A,C) = 6316 against
(B,D) = 9034: **G2f is −30.1% on the young sweep**, with within-arm spread of
23–402 µs against a 2718 µs effect.

**The whole-heap cycles are the control and they come out equal**, which is what
must happen — both features are ANDed with `young_cycle`. Range 165.8–172.2 ms
across all eight runs with no arm separating: that spread, ~3.8%, is also this
measurement's noise floor.

### And it does not move the pause, because the sweep is not the pause

`young total_us` is 111.97 ms (A) against 112.53 ms (B): **−0.5%, inside the noise
floor.** A 30% cut to a term worth 5.6% of the pause is 1.7%, and 1.7% is not
visible here.

**This is where §3b's diagnosis was wrong, and the correction is the finding.**
§3b said the split "does not pay, because `sweep` is 182 ms of a 309 ms pause and
walks every registered object whatever the split says", and promoted "a real young
space, reclaimed by resetting a cursor" to *the thing that makes Phase G worth
having*. On a **young** cycle, after G2a/G2d:

| phase | young cycle | share |
|---|---:|---:|
| `mark_us` | ~101–105 ms | **~90%** |
| `sweep_us` | 6.3 ms | 5.6% |
| `snapshot_us` | 2.7 ms | 2.4% |
| `markend_us` | 0 | — |

The 182 ms figure was a mean over all cycles and belongs to the whole-heap ones
(`full sweep_us` is still 170 ms of a 264 ms pause, i.e. 64%). Generalising it to
young cycles pointed the whole G2 programme at a term worth 5.6%.

### The real reason Phase G does not pay here: the young mark costs MORE than a full mark

| | mark |
|---|---:|
| young cycle | **100.8–105.1 ms** |
| whole-heap cycle | **90.6–92.2 ms** |

A young cycle's trace is **~11% more expensive than a full trace**, and the full
cycles had *more* registered objects when they ran (14.65M against 11.78M), which
makes the gap wider than it looks.

`remembered_roots = 19,200,000` over 24 young cycles is **exactly 800,000 per
cycle — exactly the retained set.** Every old object is a remembered-set root on
every young cycle, so the young trace covers the whole old generation *and* pays
for the card machinery on top: collect the roots, scan them, re-dirty the ones
whose edge still points into young.

**That is correct behaviour on an adversarial workload, not a defect.** The probe
rewrites a reference in the retained set every round, on purpose — §3's G0 says
why: without those stores the remembered set stays empty and a young cycle is
trivially correct with no cards at all, which is the vacuous configuration the
probe must not be. But it means **this probe cannot show a young-mark saving**, and
the structural point stands beyond the probe: the card is per-object and never
coarsens, so any workload that touches most of its old set between collections
turns a young mark into a full mark plus overhead.

### What this means for the sequencing

* **G2f stays** — −30% on a real term, no cost, and the majors are untouched.
* **G2e stays but measures zero**, and the honest reason is that **G2d already took
  the win**: a bounded nursery bounds the garbage a young cycle reclaims, so the
  memset volume it removes is 17 MB per cycle rather than the ~900 MB predicted
  above. It is kept because the redundancy argument is sound and the volume scales
  with dead-object *size* — a workload of large dead arrays would see it where a
  workload of 64-byte nodes cannot.
* **A cursor-reset young space is no longer the top of the G2 list.** It attacks
  5.6% of a young pause. The 90% is the mark, and the lever on the mark is the
  **remembered set** — card coarsening, or a summary that does not re-root the
  whole old generation when most of it has been written. That is a new item and it
  is not in this plan yet.
* Every arm reported `BAD=0 OK` with `written_intact` intact, so the correctness
  channel held throughout. Note that G2e does **not** blind that check: a
  header-zeroed corpse has `num_slots=0`, `check_field_index` refuses every index,
  `get_field` returns null, and `report_corpse_read` fires — so the detector is
  *louder* than the silent zero-read the body memset gave it. That was the missing
  step in G2e's argument and it is now checked rather than assumed.

---

## 3e. C5's `perf record`, and the largest cost in the marker is not in the marker

The step §3c asked for, taken 2026-08-17 on `BigLive 4000 250`, `-Xmx1500m`,
`CRATONVM_ZGC_RELOCATE=0`, `perf record -g --call-graph dwarf`, **with the
zero-worker serial arm recorded too** — a profile with no control names whatever
is biggest rather than whatever is *different*.

| symbol | 0 workers (serial) | 1 worker |
|---|---:|---:|
| `native_collections::gc_overlay_roots_for_collection` | **12.14%** | **12.78%** |
| `external_roots::external_roots_for_owner` | **16.50%** | 7.52% |
| `ZHeapMarkBridge::try_mark` | — | 7.14% |
| `ZMarkWorker::drain::{closure#0}` | — | 4.89% |
| kernel, on the mark threads | — | ~6.8% |

**Between them 20–29% of samples on both arms.** `external_roots_for_owner` is
the caller and `gc_overlay_roots_for_collection` the callee, split differently by
the inliner on each arm — so read the pair, not either number. This is called
**once per marked object**, from `ZgcRealHeap::visit_refs` and from
`collect_garbage`'s serial loop alike, and it takes a `std::sync::Mutex` and
hashes the owner address every time.

**It is the fourth instance of the pattern §4's item 1 lists three of**, and it
survived that cleanup for a structural reason worth keeping: the other three were
found by reading `gc/`, and this one lives in **another crate**, reached through a
provider indirection. Reading `gc/` could not have found it.

### The obvious fix is inert, and it was measured before being believed

An empty-index latch — the fix the other three got — was built, and it **does not
work here**: the profile still showed the symbol at 5–14% with the latch in. The
premise is false. `widened_obj_key` calls `register_overlay_owner_key` on **every**
native-backed collection operation, and the JDK bootstrap alone performs enough of
them that `overlay_owner_keys` is non-empty from startup onwards. "This heap has
no native collections" is not a state a real run is ever in.

Not merged, deliberately. An optimisation that is on and inert reads exactly like
a missing one, and shipping it would have made the 20–29% look addressed.

### What works: gate on the owner's CLASS — **BUILT 2026-08-17**

A Bloom filter over owner *addresses* was the obvious next idea and it is also
wrong: membership is per **instance**, so a long-running app sets every bit and
the filter degrades to "always maybe", and addresses are recycled, so removal
cannot clear bits without risking a false negative.

Gate on the **class** instead. The set of classes that can own an overlay is a
handful of native-backed `java.util` collections; it does not grow with instance
count, so it cannot saturate, and class ids are stable, so address reuse is
irrelevant. `roots_for_owner` already receives the owner's current class id — it
was added for the stale-owner identity check — so the gate needed no new
plumbing on the GC side. A 1024-word bitmap covers 65,536 class ids in 8 KB.

**It cannot produce a false negative.** Bits are only ever set, never cleared, and
the bit is set **before** the owner is inserted into `overlay_owner_keys`. A
reader that misses the bit therefore ran before the insert it would have been
looking for. An owner registered with no class id sets
`OVERLAY_OWNER_CLASS_UNKNOWN` and disables the gate wholesale — with no class to
test, "maybe" is the only sound answer. Dropping an overlay edge frees the
contents of a live collection, so the direction of error is the design.

**Engagement, measured — and this is the number the previous attempt could not
produce.** A real run (`GateProbe`: a 200k-node chain of an ordinary class plus 64
live `HashMap`s, real provider registered):

```
[GC] zgc-overlay-gate: provider=native-collection-overlays hits=28805 misses=1478 disabled=false
```

**95.1% of per-object lookups answered without the mutex**, and `misses=1478` is
the half that matters: the overlay-capable classes still fall through, so the gate
is not gating away something it must not. `disabled=false` — the fail-safe never
tripped. Identical shape on a second run at a different heap size.

**What is NOT measured is the throughput gain.** That needs an interleaved profile
on a quiet host; the box was at 31 GB used of 31 GB with 16 other `rustc`
processes, and OOM-killed the build. The engagement figures above are counts, so
they are the half that a loaded host cannot corrupt.

### The leading hypothesis for the C5 gap: the parallel marker dispatches VIRTUALLY, per object

Not measured as a cause yet — stated here with the evidence so the next attempt
starts from a hypothesis rather than from the whole file.

**The profile shows the shape.** `try_mark` (7.14%) and `visit_refs` (2.26%)
appear as their *own frames* on the one-worker arm and on no other; on the serial
arm the same work is folded into `collect_garbage` (12.62%) and
`enumerate_references` (1.46%). A symbol that exists on one arm and is inlined
away on the other is a dispatch difference, not a work difference.

**The source says why.** `ZMarkShared` holds `ctx: Arc<dyn ZMarkContext>` and the
drain loop takes `let ctx: &dyn ZMarkContext = &*self.shared.ctx`. So per marked
object the parallel path pays:

* a virtual `visit_refs`, which for the real heap is `ZHeapMarkBridge` — itself a
  wrapper, so it is **two** indirect hops to reach `ZgcRealHeap::visit_refs`;
* a virtual call **per reference**, because the child sink is
  `f: &mut dyn FnMut(u64)`;
* a virtual `is_in_heap` and a virtual `try_mark` per child.

None of them can be inlined. The serial marker calls the concrete
`ZgcRealHeap` methods directly and the optimiser inlines the lot — which is
exactly why its work does not appear as separate symbols.

That is a fixed per-object cost that **one worker pays in full and shares with
nobody**, which is the property §3c's numbers demand of any explanation: a single
worker contends with nothing and is still +98%.

**The fix has a known shape**: monomorphise. `ZMarkShared`/`ZMarkWorker` become
generic over `C: ZMarkContext` instead of holding `dyn`, and the child sink
becomes `impl FnMut(u64)`. It is mechanical but it is not small — `mark.rs` is
~4,600 lines and the type parameter reaches the coordinator, the controller and
`zgc_concurrent`.

### The cheap experiment was run, and the dispatch hypothesis did NOT survive it — 2026-08-18

`CRATONVM_ZGC_MARK_CTX_DIRECT` hands the coordinator the heap's own `Arc`
instead of `ZHeapMarkBridge`, removing **one of the two indirect hops** on every
`try_mark`, `visit_refs`, `is_in_heap` and `object_size`. One binary, three arms,
interleaved, order reversed on alternate reps, 6 runs per arm, `BigLive 3000 200`:

| arm | mean `mark_us` | vs serial |
|---|---:|---:|
| serial (`PARMARK=0`) | 47,052 | — |
| parallel + bridge | 61,976 | +31.7% |
| parallel + **direct** | 63,453 | +34.9% |

**Halving the indirect calls on the two hottest methods moved nothing** — the two
parallel arms are 2.4% apart with within-arm spreads of ±7%, i.e.
indistinguishable. Dispatch is not where the C5 cost lives.

The change is kept anyway, and **not for performance**: it deletes a
`*const ZgcRealHeap` with a hand-written `Send`/`Sync` and a three-fact soundness
argument, replacing it with an `Arc` that keeps the heap alive by construction.
The wrapper remains as the fallback for a `with_capacity` heap, which has no
self-`Arc`. Perf-neutral, safety-positive; the flag stays so the next person can
re-run the A/B rather than re-derive it.

### What the cost IS: per object, not per cycle

Same binary, same interleaving, three live-set sizes — because a per-cycle setup
cost (pool construction, thread spawn/join, the terminator handshake) does not
scale with the live set and a per-object cost does:

| `BigLive` | serial `mark_us` | parallel `mark_us` | delta |
|---|---:|---:|---:|
| 400×60 | ~3,759 | ~5,294 | ~1,535 |
| 1200×120 | ~13,453 | ~15,439 | ~1,985 |
| 3000×200 | ~45,650 | ~58,302 | ~12,652 |

Fitting `delta = fixed + k × serial` across the smallest and largest points gives
**k ≈ 0.27 and fixed ≈ 0.5 ms**. So the per-cycle setup — the thing that spawns a
pool and a driver thread per collection — is worth about half a millisecond, and
**~27% is proportional to the objects marked**. At any realistic live set the
proportional term is the whole story, which also rules out "it spawns threads per
cycle" as the explanation.

Indicative rather than settled: three reps on a loaded developer machine, and the
within-size spread is larger than the between-size differences. What it is good
enough to do is *order the suspects*.

**So the remaining suspect is the per-object WORK, not the per-object CALL.** The
serial marker pushes children onto a plain local `Vec` and sets the mark bit
directly; the parallel one publishes into striped queues behind mutexes and marks
through a CAS that must be atomic because other workers may race for the same
object. That is real work the serial path does not do, it is per object, and it
does not go away with one worker — which is exactly the property §3c's numbers
demand. The plan already listed the striped queues as a suspect; this promotes
them from "a candidate" to "the leading one".

**The original cheap-experiment note follows.**

**Cheap experiment first, before that refactor.** `ZHeapMarkBridge` adds a
*second* indirect hop for no reason other than to hold a `&ZgcRealHeap` —
`mark_with_controller_stw` builds `Arc::new(ZHeapMarkBridge { heap: self })` and
every call goes bridge → heap. Removing that one wrapper is a small change, and if
indirect dispatch is the cost it should move the needle measurably on its own. If
it moves nothing, the monomorphisation hypothesis is wrong and the remaining
suspects (the striped queues, the per-cycle pool construction) get their turn.

### And the C5 gap itself is still open

`try_mark` (7.14%), the worker `drain` closure (4.89%) and ~6.8% of kernel time on
the mark threads exist only on the parallel arm — the marker's structure, not
contention, which is what §3c concluded from timings and this confirms from a
profile. The overlay cost above is **not** the C5 gap: it is paid equally by both
arms. Fixing it makes every ZGC collection faster and leaves parallel-vs-serial
exactly where it was.

### A note on the measurement itself

The wall-clock A/B of the two binaries is **not reported here, because it is not
usable**. The host went from load 1.8 to load 18 mid-run (another session started
a benchmark and two `rustc`), and the arms were ordered old-then-new in every rep,
so drift and order are confounded with the change. `perf record --call-graph
dwarf` also inflated a 2.2 s run to 13–36 s, which is the overhead and not the
binary. The profile shares above are used instead precisely because a **symbol
share is structural**: it cannot be moved by the neighbour benchmark.

---

## 3f. The sweep reductions applied to EVERY cycle — the first measured default-config gain, 2026-08-18

G2e and G2f landed on 2026-08-17 ANDed with `young_cycle`, for one reason: they
went in mid-gauntlet and a default run had to stay byte-for-byte unchanged.
Neither argument was ever young-specific — the body is unreachable behind a zeroed
header whatever the cycle kind, and the walk is ascending on both paths. §3d then
showed the restriction was pointing them away from the cost:

| | `sweep_us` | share of pause |
|---|---:|---:|
| young cycle | 6.3 ms | 5.6% |
| **whole-heap cycle** | **170 ms** | **64%** |

A default run performs *only* whole-heap cycles. The −30% was being applied to
the 5.6%.

### Measured with the restriction lifted

One binary, four arms, interleaved with the order reversed on alternate reps,
`ZgcGenProbe 400000 30000 300`, `-Xmx1200m`, **generational OFF** — i.e. the
default configuration. 8 whole-heap sweeps per arm.

| arm | mean `sweep_us` | vs off | range |
|---|---:|---:|---|
| both off | 711,110 | — | 568k–863k |
| header-zero only | 535,354 | **−24.7%** | 435k–590k |
| dead-run merge only | 256,190 | **−64.0%** | 188k–334k |
| **both on (the default)** | **216,850** | **−69.5%** | 176k–291k |

**Whole pause: 879.6 ms → 360.6 ms, −59.0%.** `BAD=0 OK` on every run.

The arms do not overlap — the worst "on" run (291k) is better than the best "off"
run (568k) by a factor of two — so the separation survives the wide wall-clock
variance of a loaded developer machine, which is exactly why the comparison is
one binary and interleaved.

### And it explains §3d's zero

On a **young** cycle the header-only zeroing measured *nothing*, and §3d's
explanation was that G2d had already capped the memset volume: a bounded nursery
bounds the garbage a young cycle reclaims. That prediction is now confirmed from
the other side — on a whole-heap cycle, which has no such cap and reclaims 13.0M
dead objects against a young cycle's 137k, the same switch is worth **−24.7%**.
The feature was never inert; it was being measured on the arm that could not show
it.

The run merge dominates either way (−64% alone, −69.5% with the memset reduction
on top; they overlap because both cut per-dead-object work).

### What this changes

**This is the first measured gain in this plan that a default run actually
gets.** Everything else built here — concurrent marking, parallel marking,
generational — is opt-in and has so far measured negative or neutral. Both
switches are default-on and now apply to every cycle, so no flag has to be flipped
to collect it.

Renamed with the restriction: `CRATONVM_ZGC_GEN_HEADER_ZERO` →
`CRATONVM_ZGC_SWEEP_HEADER_ZERO` and `CRATONVM_ZGC_GEN_DEAD_RUNS` →
`CRATONVM_ZGC_SWEEP_DEAD_RUNS`. The `GEN_` prefix described the day they were
young-only and would now be a lie; they are a day old and nothing depends on
them.

---

## 4. Sequencing, and what to do first

```
C1 safepoint ──┬─> C3 resurrection ─> [concurrent mark works]  DONE 08-16
C2 ownership ──┘                              │
                                              v
                              C4 per-thread buffers (throughput)  OPEN

G0 card barrier on the accessor ─> G1 young cycle    DONE 08-17
                                       │
                                       v
                     G2 real young space + ZPageAllocator   OPEN
                       (and it is what `sweep_us` needs)
```

**C1 and C2 are independent and are both on the critical path.** C2 is the
larger diff and the smaller risk; C1 is the smaller diff and where a mistake is
a missed object. Doing C2 first gives C1 somewhere to put the pool.

**Phase C before Phase G**, for the reason the original plan gave for
concurrency before relocation: a bug in concurrent marking costs throughput or
surfaces as a missed object under a test, while a bug in a young cycle's sweep
frees live old objects.

### What is still open, 2026-08-17, ranked by what the measurements say

1. **C5 — make the marker scale.** All three per-object locks are fixed
   (2026-08-17) and **it was not enough — see §3c.** A fourth thing was fixed the
   same day and is also not enough on its own: **the driver's fixed-point wait
   was polling a condvar nothing notified**, on a 5 ms grid, so every pass paid an
   interval it did not need to. Worth ~5–10 ms of a ~100 ms gap, but it was a
   *floor* under the pause that worker count could not reach, and a 5 ms quantum
   that every timing in §3c was carrying. `[GC] zgc-mark-wait: park_timeouts=`
   is nonzero if it returns. **The next step is still `perf record` on a
   one-worker cycle**, and it now measures the marker rather than the marker plus
   a poll interval. One worker is already +98%
   to +189% against zero, and a single worker contends with nobody, so the
   remaining cost is the engine's fixed overhead and not contention. The next
   step is `perf record` on a one-worker cycle. The three locks were:
   `metadata_pin::roots_for_loader`, which took its registry `RwLock` and cloned
   a `Vec` for **every** marked object — its own module already had a
   `snapshot()` written for exactly that reason, which nothing called — now
   behind the `NON_EMPTY` latch its two siblings always had;
   `external_roots::snapshot()`, which **cloned the provider `Vec` per object**,
   now a `PROVIDER_COUNT` latch plus in-place iteration under
   `read_recursive`; and `concurrent_mark_skip_set`, an `RwLock` read plus an
   `Arc` clone *and drop* per object — three contended atomic RMWs — now behind
   a **Bloom filter** over the skip-set addresses, because the "is it empty?"
   trick that fixed the other two cannot work there: the skip set is non-empty
   during every cycle in a real run, since it is every registered `Reference`
   object. A filter works because the question is per *object* and almost no
   object is a `Reference`. The serial marker paid all three too, uncontended,
   which is why they are kept even though they did not close the item.
2. **G2 — a real young space.** **G2a/b/c/d landed, and so do G2e/G2f
   (2026-08-17), which are the two that attack `sweep_us` without moving
   anything.** Read §3's G2e/G2f first: the young sweep's two remaining
   per-dead-object costs were both O(reclaimed *volume*) — a memset of every dead
   body, redundant with the memset `alloc_raw` already does on every handout, and
   a free-list push per object followed by a sort over all of them — which is why
   bounding *which* objects the sweep visits (G2a) could not move the number.
   **And the critical path for the remaining, moving half is now named
   correctly**: not the ZGC load barrier (stage (a) landed 2026-08-13) but the
   **moving-young coverage proof**, which `gen_heap` has and ZGC consults zero
   times. Without it a promote-by-copy young cycle is refused whenever a compiled
   frame is live — 64 of 68 cycles on a JIT-saturated run, measured — so it would
   divert to the sweep it exists to remove. See §3's G2 for both.

   The older text of this item follows, because its measurement still stands.
   **G2a and G2b landed** — the nursery floor makes a
   young sweep O(young) (§3's G2a), and the slide now promotes its survivors into
   the old region and leaves the nursery empty (§3's G2b), which is promotion by
   copy. Neither needed the page allocator, and **neither needed the JIT load
   barrier — stage (a) of that landed 2026-08-13**, which this plan had wrong.
   What is left is young-space *allocation*: today the nursery is wherever the bump
   cursor happens to be, so an object the free list places below the floor still
   waits for a major, and the young region is not reclaimed by a cursor reset but
   by a bounded sweep. Promoted by §3b from "the answer to `sweep_us`"
   to **the thing that makes Phase G worth having at all**: with the split
   engaged and 4.8M objects skipped per young cycle, the pause did not move,
   because `sweep` is 182 ms of a 309 ms mean pause and is O(registry) whatever
   the split says. A young space is reclaimed by resetting a cursor, which is the
   only construction that removes that. Needs the page allocator; §3's G2 has the
   five-step list and why step 3 depends on the JIT load barrier.
3. ~~**`snapshot_us`**, 13% of the threaded concurrent pause~~ — **FIXED
   2026-08-17.** `bases()` materialised a `Vec` of every registered base (10.8M ×
   8 B = 87 MB allocated *inside* the pause, then walked two or three times);
   `for_each_base` scans the bitmap in place instead. Order is a correctness
   property, not a detail: the sweep hands adjacent dead spans to
   `add_free_block` and the coalescer only sees them as adjacent because the walk
   is ascending — get that wrong and the arena exhausts with most of itself
   unreachable on the free list, which is a measured failure here (`CopyChurn` at
   `-Xmx256m`).
4. **C4 — per-thread mark buffers.** Two of three parts done. The batching was
   done on 2026-08-16 (`hand_satb_batch_to_the_marker`); the **shared counters
   came off the per-store path on 2026-08-17**, which was the part that mattered
   more than it looked — `ZMarkIngress` buckets its queues across 16 mutexes so
   mutators do not contend, and then every push did a `fetch_add` on one
   `pending_hint` cache line *and* one on the caller's own counter. Striping N
   locks behind a single shared counter is not striping. The counters now live
   inside each bucket's mutex, which the push already holds.

   **SEQUENCE THIS AFTER C5, 2026-08-17.** §3c measured the marker at **+98% with
   ONE worker**, which contends with nobody — so the engine's fixed overhead, not
   handoff, is what the marker costs. A per-thread buffer makes *mutators* hand
   off faster into a consumer that is already the bottleneck, and the current
   handoff is not obviously the problem either: `ZMarkIngress` buckets across 16
   mutexes keyed by thread, and since 2026-08-17 the counters live inside the
   bucket mutex the push already holds, so a mutator's cost is one uncontended
   lock in ≤16-thread workloads. Deliberately **not** built on 2026-08-17 for that
   reason, and because it carries a use-after-free with no measurement to justify
   taking it — see the `ZMarkHandle::new_buffer` rule below.

   What remains is the genuine per-thread buffer, and it needs thread-keyed state
   on the heap because `satb_pre_barrier` is reached with **no thread context at
   all**. **Must use `ZMarkHandle::new_buffer`, never `ZMarkMutatorBuffer::new`**
   — the latter is detached, and a detached buffer dropped non-empty leaves
   objects marked-and-unscanned, which is a use-after-free because the mark bit
   is what dedups them. It is throughput on a path that is armed only during a
   concurrent cycle, i.e. only when `CRATONVM_ZGC_CONC_START` is set.
5. **A concurrent sweep.** Its own project, not in Phase C as written, and
   largely superseded by G2: a young space reclaimed by resetting a cursor has
   no sweep to make concurrent.

---

## 5. What this plan does not do

**It does not propose making anything default-on.** Every property here exits
on a measurement, and the two properties that *are* built —
`CRATONVM_ZGC_PARMARK` and `CRATONVM_ZGC_RELOCATE` — should have their gauntlet
numbers in hand before a third and fourth join them. Flipping four switches at
once produces one result and four candidate explanations.

That now covers four switches, not two: `CRATONVM_ZGC_CONC_START` and
`CRATONVM_ZGC_GENERATIONAL` are both off by default for the same reason, and
each has an engagement counter on the `--verbose:gc` line (`mark=`, `gen=`) so a
run that did not exercise the feature says so instead of being read as evidence
about it.
