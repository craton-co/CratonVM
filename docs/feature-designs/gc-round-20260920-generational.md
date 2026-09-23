# Generational collector and card table — proposals from the 2026-09-20 GC round

Scope: `gen_heap.rs`, `gen_evac.rs`, `old_gen.rs`, `young_mark.rs`,
`evac_pool.rs`, `tlab.rs`, `card_table.rs`. Everything below is sized and names
its first step. Ordered by (value ÷ risk).

---

## 1. Put the young collection's other three parallel phases on `EvacPool` — `HIGH VALUE, SMALL`

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

---

## 6. Retire the buffered card-marking pipeline — `LOW VALUE, MEDIUM`

> **SUPERSEDED 2026-09-21.** The pipeline is KEPT (that decision is the round-2
> resolution of `gengc-oldgen-dead-buffered-card-pipeline-20260920.md`), and
> the two latent hazards this entry cites as a reason to delete it are now
> FIXED instead: `clear_all` folds the pending queue rather than dropping it,
> and the buffered push carries a last-card filter. Retired to
> `docs/internal/gc/gen-card-table-latent-hazards-20260920-RETIRED-20260921.md`.
> The "why it is not obviously right to delete it" paragraph below still
> stands, and is now the whole answer.

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
