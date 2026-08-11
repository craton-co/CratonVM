# OAuth2 issuer-URI test: a young GC pause longer than the 500 ms read budget — FIXED 2026-08-11

**Status: FIXED 2026-08-11.** Supersedes
`oauth2-issuer-uri-mock-server-read-timeout-flake-20260803.md`. That page had
already root-caused the flake correctly — a stop-the-world young collection of
500–1538 ms against a 500 ms socket read budget, with MockWebServer running
*inside the same VM* — and had landed most of the pause breakdown. What it left
open was the one lever that actually decides the pause, and that lever turned
out to be **inert**.

## Where it stood

The old page's own summary: `promotion_stats` and `pointer_map_rebuild` removed
outright (both 0 ms), overlay-root locking batched (`828f8a68b` + `8e8446104`),
the pause fully accounted for phase by phase, and `cheney_drain` at ~60% of a
424 ms median pause with the conclusion that *"the lever is how many objects it
copies, i.e. young sizing (`CRATONVM_GC_YOUNG_PAUSE_MS`, implemented, default
OFF)"*.

## What was still wrong: the lever did not reach the collector

`adapt_young_trigger_to_pause` writes its adapted value into
`young_gc_threshold`. `young_gc_trigger_bytes` read it **only on the moving
branch**, and the branch is chosen by `next_young_gc_is_guaranteed_non_moving`
— which, on any JIT-heavy workload, answers "non-moving" at essentially every
allocation because an unregistered compiled frame is on the stack.

`CRATONVM_DBG_YOUNG_TRIGGER=1` on the 6-lane repro, with the goal armed at
200 ms:

```
[young-trigger] n=8192  cursor=6MB  live=6MB  threshold=460MB non_moving=true
[young-trigger] n=94208 cursor=29MB live=29MB threshold=460MB non_moving=true
```

`threshold=460MB non_moving=true` for the whole run, while `[gcpause]` in the
same run reports

```
[gcpause] young trigger 262144KB -> 131072KB (pause=498ms goal=200ms)
[gcpause] young trigger 131072KB -> 65536KB  (pause=423ms goal=200ms)
[gcpause] young trigger  65536KB -> 32768KB  (pause=373ms goal=200ms)
```

and every collection still arriving with `young_bytes_before` ≈ 272 MB. The
feedback loop was moving a number nothing read. **The prediction is not even
right**: those cycles run the MOVING path — they print `cheney_drain`.

This is the shape the old page itself warned about twice and did not apply to
its own fix: *print what the arm CHANGED beside what it COST*. It reported the
trigger adapting `262144KB → 32768KB` as evidence the fix worked, without
checking that anything consulted the adapted value.

## Fix

* **`young_gc_trigger_bytes` takes a `pause_goal_armed` flag.** When a goal is
  armed the adapted value bounds *both* branches — downward only, so it can
  never raise the non-moving trigger. With no goal the behaviour is byte-for-byte
  what it was.
* **Heap expansion stopped clobbering it.** The expansion path reset the
  threshold to `new_cap * YOUNG_GC_THRESHOLD_PERCENT / 100` unconditionally,
  undoing the adaptation on every growth. With a goal armed it now takes the
  smaller of the two.
* **The goal defaults to 200 ms** (`DEFAULT_YOUNG_PAUSE_GOAL_MS`), matching
  `G1CollectorConfig::max_gc_pause_ms` and HotSpot's `-XX:MaxGCPauseMillis`.
  `CRATONVM_GC_YOUNG_PAUSE_MS=0` opts out. A generational young collector with
  no pause target at all was the outlier.

## Measured, interleaved A,B,B,A

6 lanes per block, `-XX:+UseGenerationalGC`, `--Xmx 2g`, `CRATONVM_DBG=gcpause`
(prints only when the total pause is ≥100 ms). A = goal off, B = goal 200:

| block | arm | n | median | p90 | max | over the 500 ms budget |
|---|---|---:|---:|---:|---:|---|
| 1 | A off | 12 | 562 | 714 | 714 | **9/12** |
| 2 | B on | 79 | 217 | 585 | 842 | **10/79** |
| 3 | B on | 92 | 209 | 541 | 923 | **11/92** |
| 4 | A off | 12 | 542 | 617 | 617 | **10/12** |

Both B blocks land below both A blocks, so the separation survives the
ordering. The share of collections over the read budget goes 75–83% → 12–13%.

**No claim is made about the absolute max**, and the honest reading is that it
got worse: arm B collects far more often, so it has more chances at a bad one,
and the cycles that run before the feedback loop has seen its first pause are
still full-size. A typical post-adaptation cycle is
`young_bytes_before=34MB pre_evacuate=5ms overlay_forward=15ms cheney_drain=42ms
cardclear+young_reset=3ms` for a 134 ms total; the stragglers are the ones still
carrying 270–480 MB of young.

## Throughput control

The objection this default has on record — capping the initial young semi was a
NET REGRESSION for large-`-Xmx` workloads, which fall back to the non-moving
sweep and just fire it more often — is answered with the control it asks for,
`bench/BinTreesClassic.java`, also interleaved A,B,B,A:

| workload | goal off | goal 200 |
|---|---|---|
| bt18 `--Xmx 2g` (n=3) | mean 4.05 s | mean 4.10 s |
| bt20 `--Xmx 4g` (n=2) | mean 16.77 s | mean 17.30 s |

Both differences sit inside their own block's drift, so no throughput cost is
established. Structurally the loop cannot repeat that regression either: it only
ever reacts to a pause it measured, so a workload already inside its goal is
never touched.

## The flake itself

**0 `SocketTimeoutException: Read timed out` in 60 runs of the 6-lane repro** on
2026-08-11 — 12 under the default collector, 48 under `-XX:+UseGenerationalGC`
across every arm above. The old page's rate was ~7 in 12.

Two things changed under it, and both matter:

* **The default collector moved to ZGC on 2026-08-10.** ZGC has no moving young
  generation, so this mechanism does not exist there at all: 12/12 clean.
* **Under Generational the mechanism was real and is now bounded** by the pause
  goal, as measured above.

## A different failure this class does still have, under Generational only

3 of 12, 1 of 12, 2 of 12 and 0 of 6 runs across the arms above failed with 29
of 52 tests red and a single root cause:

```
BeanInstantiationException: Failed to instantiate [HttpSecurity]:
  Factory method 'httpSecurity' threw exception with message:
Caused by: java.lang.NullPointerException:
  Cannot invoke "String.hashCode()" because "<local4>" is null
```

It is **not** this page's defect: no read timeout is involved, it reproduces on
the pre-fix binary as readily as on the fixed one, it does not appear under the
default collector, and its rate shows no arm correlation. It reads like a
moving-young stale-reference defect and needs its own page and its own
`CRATONVM_DBG_GC_STRESS` run. Filed here only so the next person running this
repro is not surprised by a red that has nothing to do with the budget.

## Do not re-attempt (carried forward, still true)

* **Filtering the overlay walk with a `RefPredicate`** — implemented, measured
  interleaved A,B,B,A, and *no difference* (43 ms both arms). The premise that
  the phase materialises millions of `ObjectRef`s was never measured and is
  wrong; the cost is the walk, not the `Vec`. Reverted.
* **Deleting `pointer_map`** — it is a public `GcResult` field `g1.rs` composes
  across evacuation rounds, and a survivor oracle whose consumers outlive
  forwarding pointers. The 82 references in `gen_heap.rs` are not 82 copies of
  one idea.
* **`CRATONVM_CARD_TABLE_ONLY=1` alongside the goal** — `full_old_rset_scan`
  duly drops to 0 ms, typical pauses improve, and the end-to-end rate comparison
  collapses under alternation. Whether it helps, hurts, or is neutral is open
  and needs a quiet host.
* **Shrinking the repro to `IssuerBudgetProbe soak`** — 240 exchanges, 0
  failures. A slow *exchange* is not a slow *read*; the trigger needs 52 test
  methods each building a Spring context.
* **Chasing `forward_object`'s `is_forwarded()` re-encounter probe** —
  `fwd_copies` and `fwd_reencounters` are roughly 1:1, so removing re-encounters
  entirely could not account for `cheney_drain`.

## What is left, quantified

With the goal armed, a post-adaptation cycle's remaining cost is
`cheney_drain` ~42 ms, `pre_evacuate` ~5 ms, `overlay_forward` ~15 ms,
`scan_dirty_cards` ~33 ms, `full_old_rset_scan` ~31 ms. Two of those are worth a
page of their own if this collector ever becomes latency-critical again:

* `full_old_rset_scan` runs on **every** young GC by default
  (`full_old_rset_scan_enabled()` is `!card_table_only`), a full O(old-gen) walk
  that makes young-GC cost grow with old-gen size permanently. It is a standing
  insurance premium against a missed write barrier, not a bug.
* `overlay_forward` still walks every element of every overlay collection in the
  process. The remaining lever named on the old page is a **dirty/card flag on
  overlay side-table writes**, so a minor GC visits only owners mutated since
  the last cycle. Not attempted here.

Neither is on the path to the 500 ms budget any more, which is why this page
closes.
