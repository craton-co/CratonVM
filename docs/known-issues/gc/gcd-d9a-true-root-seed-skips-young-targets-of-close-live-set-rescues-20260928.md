# The true-root major never traces the young targets of an old object `close_live_set` rescues

> **STATUS (2026-09-29, gce e1/x): KEEP -- the unit tests pass, the 5/5 probe gate does not.** `gce_e1c_close_live_set_collecting_names_each_rescued_base` and the regression filters pass (e1 Windows suite). `GenR4W4NativeStringOomProbe -Xmx64m` on e1: `nsoom5_1..5` 4/5 (`nsoom5_5` DIFF), `gen_nsoom_1..2` 2/2; base `nsoom5` 5/5. Over the orchestrator's 20 runs it is 19/20 on BOTH binaries, so it is a pre-existing flake, not a regression, but the failing run's stderr was not kept. **Remaining:** capture one failing run with `CRATONVM_DBG=gc-stats,oldmark-root-census,root-source` and classify its holder. A gap inside a finalizer round's subgraph (this page's stated residual) keeps the page; any other holder retires it and moves the flake to its own page.

> **STATUS (2026-09-29, gce e1/c): FIXED IN CODE, awaiting the probe -- under the true-root seed the live-set closure now runs INSIDE the mark, before the finalizer round, and what it rescues goes back through the mark body, so the next `round` traces its young targets.**
>
> - **Confirmed by reading:** `OldGen::close_live_set` ran after the mark loop, i.e. after `TrueRootYoung::round` had reached its fixed point; a rescued old object never went through `pending_old`.
> - **Fix** (`gc/src/gen_heap.rs::old_gen_gc_inner`, `gc/src/old_gen.rs`): `OldGen::close_live_set_collecting` (the same closure, also returning the rescued bases). In the mark loop, when the strong closure (BFS + `round`) runs dry and the finalizer round has not started, and only when `true_young` is `Some`, the closure runs; the rescued bases are pushed to the worklist (already marked, so the pop runs the whole body: fields, overlays, loader / mirror / metadata pins, `pending_old`), repeated until it rescues nothing. Rescues are counted in `OLD_SWEEP_CLOSURE_RESCUES` with their own `warn`. The step-1 young reclamation stays refused on any rescue (`pre_rescued == 0` joins `rescued == 0`). The pinned compaction's Phase 0 closure then finds nothing in the strong set. Perf: when no finalizer round ran after the in-mark closure, the sweep reuses its verdict instead of a second O(live) pass (`reuse_pre_close`), so a healthy true-root major still pays one closure.
> - **Residual (narrow):** a gap inside a finalizer round's subgraph is still rescued only by the sweep's closure, after the fixed point. The in-mark closure does not run during the resurrection drain, so that it cannot put a strongly reachable object into the depth-2 closure.
> - **Tests:** `cargo test -j 5 -p cratonvm-gc --lib gce_e1c_close_live_set_collecting_names_each_rescued_base`; regressions: `cargo test -j 5 -p cratonvm-gc --lib gcd_d9a_`, `gcd_d4n_`, `gcd_d5r_`, `close_live_set`. No end-to-end planted-gap test: no switch drops a BFS edge that the closure still sees.
> - **Retire when** the tests pass and the true-root battery row is unchanged: `CRATONVM_DBG=gc-stats cratonvm --java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench GenR4W4NativeStringOomProbe` 5/5 (`fill: OutOfMemoryError "Java heap space"`, `native-strings ok`, `recovered ok`, `PASS`).

> **STATUS (2026-09-28, gcd d9/a): OPEN, filed by reading (unmeasured).
> Only reachable after a mark push-site gap, which is itself a defect the
> sweep already reports.** Owner: the old-gen lane (`gc/src/gen_heap.rs`,
> `old_gen_gc_inner`).

*Filed 2026-09-28 by gcd d9/a (trueroot9), from the review of the true-root
seed (`TrueRootYoung`).*

- **Severity:** a use-after-free candidate, conditional on another defect: a
  mark push site that dropped an edge (`rescued > 0`, the `tracing::warn!`
  "in-place old-gen sweep: the mark phase missed N of M walked block(s)",
  counter `OLD_SWEEP_CLOSURE_RESCUES`).
- **Backend:** Generational, requested majors (the true-root seed).

## What is wrong, by reading

The in-place sweep closes the live set before it frees
(`OldGen::close_live_set`, GCAUD-8): an unmarked old object that a MARKED old
object still references is marked and kept. That closure follows old -> old
reference slots only. Under the legacy young seed that was enough: every young
survivor was a seed, so whatever a rescued object reaches THROUGH young was
already marked. Under the true-root seed the young side is a fixed point over
the marked old objects (`TrueRootYoung::round`), computed BEFORE the closure:
a rescued object's young targets were never traced, and the old objects only
they reach are freed while the rescued object (and its young target) live on.

The pinned compaction's Phase 0 closure (`close_live_set_over_old_gen`) has
the same shape.

gcd d9/a's step 1 (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_FREE_YOUNG`) already
refuses to reclaim young when `rescued > 0` (and on every compaction), so the
young half of the hazard is closed; the old half is this page.

## Proposed fix

In the in-place arm, when `true_young` is `Some` and the closure rescued
anything: run one more round over the rescued objects (seed their young
targets, drain, and send what that marks through the BFS body), then close
again, until nothing changes. The BFS body is inline in `old_gen_gc_inner`;
extracting it into a `fn mark_old_object(obj, ...)` (the per-object half:
fields, overlays, loader, mirror and metadata pins) is the prerequisite.
Cheaper alternative: on `rescued > 0` under the true-root seed, re-run the
mark with the legacy seed (the collection has freed nothing yet).

## How to verify

A unit test that plants a mark gap (an old holder `H` marked, whose field
names an unmarked old `R` that the BFS is made to skip, e.g. through the
`interior_to_the_walk` screen), with `R -> young y -> old p`: after a
true-root major, `p` must still be allocated.
