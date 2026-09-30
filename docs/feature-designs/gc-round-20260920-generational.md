# Generational collector and card table — proposals from the 2026-09-20 GC round

Scope: `gen_heap.rs`, `gen_evac.rs`, `old_gen.rs`, `young_mark.rs`,
`evac_pool.rs`, `tlab.rs`, `card_table.rs`. Everything below is sized and names
its first step. Ordered by (value ÷ risk).

---

## 1. Put the young collection's other three parallel phases on `EvacPool` — `HIGH VALUE, SMALL`

> **Status 2026-09-23 (gengc round 4, lane B `sweep`): two of three phases
> CONVERTED; measurement pending.**
>
> * **Done:** `parallel_objstart_walk` and `parallel_sweep_walk` now dispatch
>   on `GenerationalHeap::evac_pool(threads)` through one helper,
>   `gen_heap.rs::run_chunk_workers` (driver runs one `worker()`, `threads - 1`
>   pool helpers run the rest, `EvacPool::ensure_helpers` first so a pool
>   built narrower cannot clamp the walk). `EvacPool`'s API was used as-is.
>   Nesting was checked: both walks run on the collecting thread outside any
>   dispatch (the object-start walk finishes before `ParEvac::drain` starts; the
>   non-moving sweep never evacuates in parallel). Unit tests:
>   `gen_r4_sweep_chunk_workers_on_the_pool_cover_every_chunk_exactly_once`,
>   `gen_r4_sweep_a_chunk_worker_panic_reaches_the_driver_and_the_pool_survives`,
>   plus the two existing object-start-walk tests now run on a pool.
> * **Side effect:** when the object-start walk engages it is the pool's first
>   user in the cycle, so the lazily-built pool is now sized at the POLICY
>   width (`young_gc_threads`) rather than the first `ParEvac::plan` width —
>   i.e. this also closes the practical half of
>   `gengc-alloc-evac-pool-width-frozen-20260920.md` part (b) whenever the walk
>   runs. `[GC] evac_pool: pool_dispatches=` now counts walk dispatches too.
> * **Not converted:** `young_mark.rs::drain_parallel` and
>   `zero_spans_parallel` (outside this lane's files). Both are called outside
>   any pool dispatch, so neither has the nesting problem; they take a
>   `threads` count, not a pool, so the change is a signature one plus the
>   same helper.
> * **The measurement this item is gated on cannot be taken on the recipe
>   above**: `[gcpause]` lines print only for pauses ≥ 100 ms, and under
>   `CRATONVM_DBG_GC_STRESS=250000` no pause reaches that — see
>   `docs/internal/gc/gengc-r4-sweep-gcpause-hides-pauses-under-100ms-FIXED-20260923.md`.
>   Until that knob exists, measure as specified in
>   `docs/internal/reviews/gengc-round4-sweep-20260923.md` ("Probes"): a
>   binary at `aa58f3a2e` vs one at the lane-B branch tip, interleaved runs,
>   medians of `objstart_walk=` (moving path, heap large enough for ≥ 100 ms
>   pauses, `CRATONVM_GC_PAR_THREADS=4`) and of `[gcphase]
>   sweep-walk-parallel-prefix` (`CRATONVM_NO_MOVING_YOUNG=1
>   CRATONVM_DBG_GCPHASE=1`). If neither median moves, record the numbers here
>   and do not convert the remaining two.

**The argument is already written down, in this lane, and it was measured.**
`gen_evac.rs`' `drain` doc says it plainly: the parallel copy phase spawned its
helpers per pause to begin with, and on a 6144-node DAG with eight workers over
twelve consecutive collections the helpers scanned **zero** destinations —
"because the driver drained the whole closure in about a millisecond while seven
freshly-spawned OS threads were still on their way to their first lock
acquisition". That is why `crate::evac_pool::EvacPool` exists: threads parked on
a condvar wake in microseconds and are never recreated.

Three phases of the *same collection* did not get the treatment, and each one
opens a fresh `std::thread::scope` on every pause:

| phase | site |
|---|---|
| the from-space object-start walk | `gen_heap.rs::parallel_objstart_walk` |
| the non-moving sweep's chunked walk | `gen_heap.rs::parallel_sweep_walk` |
| the mark drain and the span zeroing | `young_mark.rs::drain_parallel`, `zero_spans_parallel` |

On a `CRATONVM_DBG_GC_STRESS=250000` soak that is ~500–1000 collections per
process, each paying up to `young_gc_threads()` thread creations three times
over. The object-start walk is the one that matters most: it is chunked at an
8 MiB anchor stride, so on a small young gen it has a handful of chunks and the
spawn cost is a large fraction of the phase it is parallelising — which is
exactly the shape `gen_evac` measured and fixed.

**Sizing.** Small, and mechanical: `EvacPool::scope(helpers, &body, driver)`
has the same shape as the `scope(|s| { for _ in 1..threads { s.spawn(&worker) }
worker() })` block all three sites already use, and the pool is already reached
from `GenerationalHeap::evac_pool(workers)`. The awkward part is that the three
sites are free functions with no `&self`, so the pool handle has to be threaded
in as a parameter (`parallel_objstart_walk(.., pool: &EvacPool)` etc.) or
carried on `SweepCtx`.

**Risk.** The pool's completion barrier holds even when a worker unwinds
(`evac_pool`'s module note is entirely about that), and the three bodies are
already `Fn(usize) + Sync` closures. The one real difference is that
`EvacPool::scope` serialises on `dispatch`, so the mark drain cannot nest inside
the copy drain — check that before converting the mark phase.

**First step.** Convert `parallel_objstart_walk` only, behind no flag, and
report `objstart_walk` phase-mark medians from
`CRATONVM_DBG=gcpause CRATONVM_DBG_GC_STRESS=250000 … BinT 14` before and
after, on one binary. If the walk does not move, the other two are not worth
converting and this item closes with a number.

---

## 2. A cross-copy drift gate for the two evacuators — `HIGH VALUE, SMALL`

**This round found the exact defect it is for.** The serial `forward_object_impl`
and the parallel `ParEvac::evacuate` implement the *same* tenuring decision.
The parallel one has always read
`self.force_promote_all || owned.gc_age().saturating_add(1) >= self.promotion_age`.
The serial one read `force_promote_all || header.gc_age() + 1 >= PROMOTION_AGE`
— a bare `+ 1` on a `u8` that every *increment* in the file saturates at 255.
An object that keeps surviving while old gen is full parks at 255 and the next
cycle's serial copy is an arithmetic overflow: a debug-build panic inside the
collector, and in release a wrap to 0 that makes the oldest survivor in the
nursery read as the youngest and never tenure. (Fixed in this round, along with
the identical expression in the non-moving sweep's selective-promotion pass.)

Nothing would have caught that. The two evacuators have a test that asserts they
produce the same forwarding map (`the_parallel_and_serial_evacuators_agree`),
but it cannot reach age 255, and neither can any constructible fixture in
reasonable time.

**Proposal.** Extract the tenuring predicate into one `#[inline] fn
should_tenure(gc_age: u8, force_promote_all: bool, promotion_age: u8) -> bool`
in `gen_heap.rs`, call it from all three sites, and give it an exhaustive test
over `gc_age in 0..=255` × `force_promote_all` × `promotion_age in 0..=4`. 1536
cases, microseconds, and it is the only form of this test that can see the
boundary.

**First step.** The extraction; the three call sites are one line each.

---

## 3. Make the refusal path a first-class outcome — `MEDIUM VALUE, SMALL`

> **DONE 2026-09-21.** Three reason codes (one per refusal arm — the page and
> this entry both missed the to-space-undersized one), a `skipped=` term in
> every aggregate, `refusal=` on the per-cycle line, and the fix for the
> `moving_cycles_total` that counted the refusals. Retired to
> `docs/internal/gc/gen-refused-young-cycle-is-unreported-20260920-RETIRED-20260921.md`.

See `docs/internal/gc/gen-refused-young-cycle-is-unreported-20260920-RETIRED-20260921.md`.
A young collection that refuses to run reports nothing: not `cycles`, not
`coverage_fallbacks`, not `minor_gc_count`. A wedging heap and an idle heap
produce identical counters. Needs one `decision_reason` constant in
`gc_metrics.rs` (which this lane does not own) plus a two-line call before the
refusal `return`.

**First step.** The constant, and the test at `gen_heap.rs:26978`'s fixture that
proves the report can name it.

---

## 4. Sweep the young trigger, on the instrument that now exists — `MEDIUM VALUE, MEDIUM`

`CRATONVM_GC_YOUNG_TRIGGER_PERCENT` ships at its historical 50 % with the
function's own doc saying the 50 % is justified by a premise that is false:
to-space has the *same* capacity as from-space, and promotion drains to old gen
on top of that, so the copying collector's real constraint permits considerably
more. The knob exists precisely so someone can sweep it, and nobody has.

What makes the sweep worth doing *now* rather than when the knob landed: after
this round's and the previous round's work the per-collection cost is dominated
by phases that are O(young allocated) (the object-start walk) and O(young live)
(the copy). Halving the collection count halves the first outright. The risk is
the second: a higher trigger raises peak survivor volume, and a cycle whose
survivors do not fit diverts to the non-moving sweep and spills to old gen —
which `PAR_EVAC_DECLINED_SLACK` and the decision report can both now see.

**Sizing.** Medium, because it is measurement, not code: five arms
(`25/50/70/85/95`), one binary, interleaved ABBA on an idle host, on at least
two workloads with different survival rates (`bench/BinT 14`, whose survivors
are a persistent tree, and `bench/HashMapOnly`, whose are not). Report wall,
`minor=`, total pause, p50/p99 pause, `coverage_fallbacks`, and
`declined_for_slack`.

**First step.** Mine the existing A/B logs for this host's noise floor before
running anything — a 4 % wall delta at load 40 is not a result.

### Status — 2026-09-23 (round 4, lane `alloc`): instrument complete, protocol written, NOT RUN, default unchanged (50)

**What the knob actually controls — read this before the numbers.**
`CRATONVM_GC_YOUNG_TRIGGER_PERCENT` (`CRATONVM_GC=young-trigger-percent=<n>`,
clamped `1..=95`) sets the INITIAL `young_gc_threshold` and the CEILING of the
pause-goal loop (`adapt_young_trigger_to_pause`: `ceiling = capacity * pct /
100`). It reaches the two young branches differently
(`young_gc_trigger_bytes`):

| | pause goal ON (default, 200 ms) | pause goal OFF (`CRATONVM_GC_YOUNG_PAUSE_MS=0`) |
|---|---|---|
| moving cycle | live threshold (knob-seeded, loop-adjusted) | knob, fixed |
| non-moving cycle (live JIT frame, `CRATONVM_NO_MOVING_YOUNG`) | `min(live threshold, 90 %)` | **90 %, the knob is INERT** |

So an arm run with the goal off on a JIT-warm workload measures nothing, and an
arm run with the goal on measures "knob + loop". Both sub-protocols below are
needed, and `non_moving_cycles_total` on `[GC] moving_young:` says which
branch a run mostly took.

**Instrument (landed round 4).** `GenerationalHeap::young_trigger_census()` /
`young_trigger_census_line()` → one line,

```
[GC] young_trigger: young_trigger_bytes=… young_semi_capacity=… young_trigger_pct_now=… \
  young_trigger_knob_pct=… young_pause_goal_ms=… adapt_calls=… adapt_halvings=… \
  adapt_reverts=… adapt_latched_holds=… adapt_increases=… adapt_on_major=… \
  young_trigger_low_water=…
```

It needs one `eprintln!("{}", h.young_trigger_census_line())` in
`VmHeap::print_gc_summary`'s `Generational` arm (cross-lane request, `vm_heap.rs`;
add `gen_heap.rs` to — or copy the line's keys into — the key-uniqueness
test's corpus). Until that lands, the same facts come from
`CRATONVM_DBG=gcpause` (`[gcpause] young trigger AKB -> BKB` per change).
Per-cycle pause, kind and `+major` come from `--verbose:gc`'s
`[GC] generational: cycle=… kind=… pause=…ms` lines; p50/p99 are computed
offline from those (this backend keeps no pause histogram).

**Arms.** `pct ∈ {25, 50, 70, 85, 95}` — five arms, one binary, env only.

**Sub-protocol A (the knob as shipped, goal ON).** Per arm:

```
CRATONVM_GC_YOUNG_TRIGGER_PERCENT=<pct> CRATONVM_DBG=gcpause \
  <cratonvm> -XX:+UseGenerationalGC --verbose:gc -Xmx<X> -cp <bench-classes> <Main> <args>
```

**Sub-protocol B (the knob alone, goal OFF).** Same, plus
`CRATONVM_GC_YOUNG_PAUSE_MS=0`. Only meaningful on runs whose
`non_moving_cycles_total` is small.

**Workloads** (`tools/bench/`, different survival shapes):

| id | main + args | heap | shape / oracle |
|---|---|---|---|
| W1 | `BinTreesClassic 18` | `-Xmx2g` | long-lived tree + transient trees; the pause-goal flip's own A/B workload |
| W2 | `HashMapOnly 10000000` | `-Xmx1g` | monotonically growing live set, promotion-heavy; checksum printed |
| W3 | `OldGenRsetProbe 18 40 12` | `-Xmx1g` | fixed tenured set, majors; oracle `retained=137438691328 churn=17895532980` |
| W4 | `BinT 14` | `-Xmx256m` | few GCs, control; oracle `sum=327670` |

**Order and repetitions.** Idle host. For each workload, interleave the five
arms in a rotated Latin order (`25 50 70 85 95`, `50 70 85 95 25`, …) for 5
rounds = 25 runs/workload/sub-protocol; take medians per arm. First take the
noise floor: 10 runs of the pct=50 arm alone; an effect smaller than 2× the
interquartile range of those is not a result (`cratonvm-microbench-noise`: in-JVM
timings swing ~3× between reps on this host — use the process wall).

**Metrics per run** (all from the one stderr): wall (process), the workload's
own `ms=` if it prints one, `[GC] generational: minor= major=`, per-cycle
`pause=` → sum, p50, p99, max (moving and non-moving separately, `kind=`),
`[GC] moving_young: moving_cycles_total= non_moving_cycles_total=
coverage_fallbacks=`, `[GC] par_evac: declined_for_slack=`, and the
`[GC] young_trigger:` line (or the `[gcpause]` trigger trace). Correctness:
the oracle columns above must match on every run.

**Decision rule.** Move the default only if one arm beats 50 on wall AND on
p99 pause for W1 and W2, does not lose on W3 (where `adapt_on_major` and
`adapt_halvings` show the loop fighting old-gen pauses — see
`docs/internal/gc/gengc-r4-alloc-pause-goal-loop-reacts-to-old-gen-pauses-FIXED-20260923.md`,
fixed in round 4 wave 2: a `+major` pause no longer reaches the loop, and
`adapt_on_major` now counts the pauses it IGNORED — so run the sweep on a
binary that has that fix, or the arms measure the loop fighting majors), and leaves `declined_for_slack` and
`non_moving_cycles_total` (diversions) no worse. Report the table even if the
answer is "keep 50".

---

## 5. Teach `close_live_set` a worklist — `LOW VALUE, SMALL`

`OldGen::close_live_set_over_old_gen` is a re-scan-everything fixpoint: each
pass walks every marked object's reference slots, and it repeats until a pass
promotes nothing. It is O(passes × live refs) where a worklist closure is
O(live refs). On a healthy cycle `promoted == 0` and there are two passes, so
this is not a hot spot today — the value is that the cost of the *unhealthy*
case (the mark missed a long chain) is quadratic in the chain length, and the
unhealthy case is exactly when the collector is already in trouble.

**Sizing.** Small. Seed a `Vec<*mut u8>` from the marked objects, pop, scan,
push each newly-promoted referent. The admission proof (`is_walked_base`) does
not change at all.

**First step.** Not this: first add a counter for the observed pass count, so
the change has a before-number. A refactor of a fixpoint nobody has measured is
a refactor nobody can score.

> **STATUS 2026-09-23 (gen round 4, lane `oldgen`): LANDED, counter and
> worklist together — unbuilt at the time of writing; the orchestrator builds.**
>
> * **Correction to the premise:** the healthy case was ONE pass, not two
>   (`loop { ...; if !promoted_any { break } }` exits after a pass that
>   promotes nothing, and on a healthy cycle the first pass is that pass).
>   Extra passes were paid only for rescues BEHIND the linear cursor — a
>   rescued object ahead of it was already scanned in the same pass.
> * **The counter**, done so that the before-number is recoverable from an
>   after-run rather than lost with the old code:
>   `old_gen::CLOSE_LIVE_SET_{CALLS, RESCUED, MULTI_PASS_CALLS, DEEP_CALLS,
>   WORKLIST_SCANS}`. The old fixpoint ran exactly 1 pass on
>   `CALLS - MULTI_PASS_CALLS` calls, at least 2 on `MULTI_PASS_CALLS`, and
>   at least 3 on `DEEP_CALLS` (the calls whose drain rescued anything — exactly
>   the calls whose old second pass would have). Not yet printed anywhere;
>   `VmHeap::print_gc_summary` (`gc/src/vm_heap.rs`, next to `COALESCE_CALLS`)
>   is the natural home and is outside this lane.
> * **The worklist:** `OldGen::close_live_set_over_old_gen` is now one
>   address-order pass over marked objects plus a drain of the objects that
>   pass promoted behind its cursor. Every object in the final marked set is
>   scanned exactly once, so the result — marked set, rescue count, escape
>   verdict — is identical by construction, and the admission proof
>   (walked-base membership) is unchanged; the binary search now returns the
>   index, which is what tells "behind" from "ahead".
> * **The equivalence test:**
>   `old_gen::tests::the_worklist_closure_matches_the_old_fixpoint_exactly`
>   keeps the old fixpoint verbatim as an oracle and compares the two on 64
>   seeded graphs (sparse, dense with self-loops, an all-backward chain — the
>   old worst case — and a mixed shape), half of them with a referenced block
>   already freed so the escape arm is compared too.
>   `a_backward_chain_is_closed_by_one_pass_and_a_drain` pins the worst case.
> * Review page: `docs/internal/reviews/gengc-round4-oldgen-20260923.md`.

---

## 6. Retire the buffered card-marking pipeline — `LOW VALUE, MEDIUM`

> **SUPERSEDED 2026-09-21.** The pipeline is KEPT (that decision is the round-2
> resolution of `gengc-oldgen-dead-buffered-card-pipeline-FIXED-20260923.md`), and
> the two latent hazards this entry cites as a reason to delete it are now
> FIXED instead: `clear_all` folds the pending queue rather than dropping it,
> and the buffered push carries a last-card filter. Retired to
> `docs/internal/gc/gen-card-table-latent-hazards-20260920-RETIRED-20260921.md`.
> The "why it is not obviously right to delete it" paragraph below still
> stands, and is now the whole answer.
>
> **First step DONE 2026-09-23 (round 4, lane `cards`), in the form that still
> applies.** `CardTable::thread_local_dirty` carries a per-table one-shot
> `tracing::warn!` naming the pipeline the first time anything feeds it. The
> `debug_assert!(pending.is_empty())` half was not added: `clear_all` now
> folds the queue by design, and its own tests
> (`clear_all_folds_pending_into_the_clean_bitmap`) fill it on purpose. What
> remains is the run: boot the corpus and grep for the warning.

See `docs/internal/gc/gen-card-table-latent-hazards-20260920-RETIRED-20260921.md`. Roughly
250 lines — `thread_local_dirty`, `flush_dirty_buffer`, `DirtyPartitions`,
`BUFFER_REGISTRY`, `DirtyBufferGuard` — plus two process-global statics and a
thread-exit hook, with no production caller since the lock-free barrier landed.
It carries two latent correctness hazards (`clear_all` drops the pending queue;
no dedup, no bound) and one real cost this round fixed a leak in (a dropped
`CardTable` used to strand its bucket in every thread buffer forever, which also
made `DirtyBufferGuard::drop` refuse to deregister an exiting thread).

**Why it is not obviously right to delete it.** `flush_all`'s cross-thread drain
is the V6 use-after-free fix, and the argument for it — "no VM-side safepoint
flush is required for correctness" — is the property the whole design rests on.
Deleting the machinery means asserting that no future barrier will ever buffer
again, which is a stronger claim than "nothing buffers today".

**First step.** Not a deletion. Add a `debug_assert!(pending.is_empty())` to
`clear_all` and a one-shot `tracing::warn!` in `thread_local_dirty`, run the
full corpus, and see whether the buffered path is genuinely unreached or merely
unreached *on the paths the suite exercises*. That is the fact the deletion
needs and the fact nobody currently has.
