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

### G2 — promotion, and a real young space — **NOT BUILT, and re-scoped**

This item said promotion needs `ZPageAllocator` because "a logical grid cannot
separate young from old in address space". That is true and it is no longer the
blocker it was described as, because **G1 does not separate them in address
space at all** — it separates them by header age, and a non-moving mark-sweep
collector does not need young and old to be contiguous.

What is still missing, and what it would buy:

* **Moving survivors into an old space.** Today a promoted object stays where it
  is. A real young space would give the young generation contiguous, sequentially
  allocated memory and let a young cycle reclaim it by *resetting a cursor*
  rather than by sweeping the whole registry. That is the answer to
  `sweep_us` — 30–52% of the concurrent pause and the pause floor (§2c) — and it
  is the only one, because a sweep over a flat registry is O(all objects)
  whatever the generation split says.
* **A young-only sweep.** G1's sweep still walks every registered object,
  including every old one it has just pre-marked. The pre-mark makes that walk
  cheap (a flag set, no field enumeration) but it is still O(registry), so a
  young cycle's *sweep* cost does not fall with the generation split even though
  its *mark* cost does.

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
cannot be done first. Step 3's dependency on the JIT load barrier is the real
critical path, and it is a separate design
([`zgc-jit-load-barrier.md`](zgc-jit-load-barrier.md)).

---

## 3b. What Phase G actually measured — 2026-08-17

`probes/ZgcGenProbe.java`: 800,000 retained linked nodes (~134 MB live), 600
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

`probes/BigLive.java` at width 4000 / depth 250 (~1M live nodes), `-Xmx1500m`,
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
   (2026-08-17) and **it was not enough — see §3c.** One worker is already +98%
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
2. **G2 — a real young space.** Promoted by §3b from "the answer to `sweep_us`"
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
