# `TestDefaultInstanceManager.testClassUnloading` — fourth recurrence CLOSED. The last arm was the empty-object-run unwind, already fixed for something else

| | |
|---|---|
| **Status** | **CLOSED 2026-08-13.** Passes on all four collectors: no-flag default, `-XX:+UseZGC`, `-XX:+UseG1GC`, and explicit `-XX:+UseGenerationalGC` — the arm this page was left open on. |
| **Supersedes** | `defaultinstancemanager-fourth-recurrence-OPEN.md`. Its investigation stands; only the "third, not-yet-root-caused factor" is answered. |
| **The answer** | The third factor was not a third defect. It was **root cause #1's other arm**, closed on 2026-08-12 by `05368d5a9` while fixing an unrelated Hibernate Reactive livelock, and nobody connected the two pages. |
| **Regression pin** | `RClassUnloadSweepGen` — new, scheduled, and **proved to go red**. The pin this page demanded already existed and could not have caught this; see *The pin was there and was blind*. |

## What the page was left open on

The 2026-08-11 session fixed two root causes and closed with:

> **Open, Generational-specific only:** a third, still-unidentified mark-phase
> factor keeps the evicted loader reachable under explicit
> `-XX:+UseGenerationalGC` even with root causes #1 and #2 both fixed.

and named one untried hypothesis — that some unrelated live object's class
shares the per-webapp loader and pushes it via `loader_pin_addr`.

That hypothesis is not what happened, and no new instrument was needed to
settle it.

## The measurement

`CRATONVM_GC_NO_EMPTY_OBJECT_RUN=1` (added on this branch — see below) is an
in-binary A/B for the young sweep's empty-object-run recovery. One binary, one
host, back to back, `-XX:+UseGenerationalGC`:

| arm | result |
|---|---|
| recovery **on** (current `dev`) | `OK (1 test)` |
| recovery **off** (pre-2026-08-12 behaviour) | `java.lang.AssertionError: expected:<8> but was:<9>` |

That is the page's exact symptom, produced and removed by one predicate, with
nothing else varying. The supporting counters agree — `CRATONVM_DBG_MARK_WHY_CLASS=org/apache/jsp/annotations_jsp`
with the recovery on:

```
[MARKWHY] arming young-mark watch on org/apache/jsp/annotations_jsp loader=0x2a3b84d9f40
[MARKWHY] sweep enter: watch=0x2a3b84d9f40 ... in_from_space=true
[MARKWHY] disposition census: ... dead_pushed=0 unwinds=0 sites=[0, 0, 0, 0]
          unwound_entries=0 dead_regions_final=30649 side_sorted=142960
[MARKWHY] sweep done: objects_swept=401719 bytes_swept=89387712
          dead_regions=30649 reclaimed_regions=30649
```

against this page's own last measurement of `sites=[0, 73, 0, 0]` and
`dead_regions_final=55`. **No `[MARKWHY] young marker reached … via …` line at
all** — the marker never marks the evicted loader, so the `is_marked=true` the
2026-08-11 session chased is gone with the unwinds. `CRATONVM_DBG_MIRRORPIN_WHY=1`
confirms it from the other end: `reconcile` fires for the two genuinely live
JSPs (`bug36923_jsp`, `bug5nnnn/bug51544_jsp`) and **not** for
`annotations_jsp`, and `live_instances_of_this_loader=1` for a live loader —
which also retires this page's "reports 0 for all three loaders, including the
known-live controls" as a second, now-absent symptom of the same thing.

## Why it was the same root cause, and why that was easy to miss

Root cause #1's fix (2026-08-11, on this page) added `side_sorted` as the
discriminator for the sweep's all-zero-span anomaly screen: a span the marker
vouches for is a live never-hashed object, not anomaly evidence. That is
correct, and it is **half** the population.

The other half is a *dead* empty object. `ClassId(0)`, kind `Object`,
`num_slots = 0`, hash not yet minted — `HEADER_SIZE` all-zero bytes, sized at
exactly `HEADER_SIZE`, and the JIT's `new Object()` fast path leaves the mark
word zero (the interpreter mints the hash eagerly, which is why `--nojit` never
showed this). Being dead, it is unmarked, so `side_sorted` **cannot** vouch for
it — by construction the 08-11 fix leaves it on the anomaly path, which unwinds
every reclaim decision taken since the last grid anchor. The evicted JSP
loader's own reclaim decision was one of the ~27 000 discarded, so its span was
never zeroed and `is_live_young_survivor` (`word0 != 0`) answered "live".

`05368d5a9` (2026-08-12) closed exactly that arm with
`zero_run_is_empty_object_run` / `zero_run_verdict`: an all-zero run that is a
whole number of `HEADER_SIZE` slots, with no marked base strictly inside it and
a plausibly-sized header at `run_end`, is a run of empty objects and the walk
steps over it on-grid instead of unwinding. It was found from a
`BatchingConnectionTest` young-GC **livelock** under `-XX:+UseGenerationalGC` —
a completely different symptom of the same discarded-reclaims mechanism — so
neither page cites the other.

The connection is worth stating in the general form, because this family has
now produced four Tomcat recurrences and one Hibernate hang: **the anomaly
screen's two halves are "all-zero and vouched-for" and "all-zero and not", and
a fix for either one alone leaves a population that looks like a fresh defect.**

## The pin was there and was blind — twice over

This page's own closing instruction was:

> **Do not close this without a regression pin.** … Whatever closes this needs
> a vector that runs in `regression-suite` — not a `probes/` reproducer that
> nothing schedules.

That was done: `RClassUnloadSweep` was written and scheduled in `CORE_CLASSES`.
It could not have caught this, for two independent reasons, both measured on
2026-08-13 rather than reasoned about:

1. **Wrong collector.** The suite has no GC arms — every vector runs on
   whatever the default is, and that default flipped Generational → ZGC on
   2026-08-10. With the recovery disabled, `RClassUnloadSweep` still reports
   `PASS` on the default, because the ZGC path genuinely does not have this
   sweep.
2. **Wrong allocation shape.** Its `churn()` allocates `byte[128]`, whose header
   carries the array length and is therefore never all-zero. With the recovery
   disabled AND forced onto `-XX:+UseGenerationalGC`, the un-strengthened vector
   still printed `unloaded=true` — it passed on a VM carrying the exact defect
   it existed to pin.

Both are closed:

* `RClassUnloadSweep.emptyChurn` allocates bare `new Object()` from a
  deliberately-warmed (JIT-compiled) loop — the only shape that manufactures
  the run — and keeps one per 1024 so the loop is not optimised away wholesale.
* `RClassUnloadSweepGen` is that same probe (it calls
  `RClassUnloadSweep.unloadedUnderChurn()` directly, so the two cannot drift)
  scheduled a second time with `-XX:+UseGenerationalGC` supplied by `run.sh`'s
  `class_cv_args`. `$cvextra` never reaches the HotSpot oracle, which is right
  here: the claim is "a real JVM unloads this class", not "under a named
  collector".

**Proved red, not assumed.** With `CRATONVM_GC_NO_EMPTY_OBJECT_RUN=1`:

```
  RClassUnloadSweep PASS                       <- default collector: cannot see it
  RClassUnloadSweepGen FAIL  output differs from HotSpot
        --- HotSpot ---   CK RClassUnloadSweepGen payload.class.unloaded=true
        --- CratonVM ---  CK RClassUnloadSweepGen payload.class.unloaded=false
```

and with the recovery on, both PASS. The `RClassUnloadSweep PASS` line in the
red arm is not noise — it is the evidence that the pre-existing pin was the
wrong instrument, kept in the record so the next session does not re-derive it.

## The A/B lever this needed and did not have

`CRATONVM_GC_NO_EMPTY_OBJECT_RUN=1` (`gc/src/gen_heap.rs`,
`empty_object_run_recovery_disabled`) makes `zero_run_empty_object_resume`
return `None`, restoring the pre-2026-08-12 unwind. All six call sites go
through that one helper, so the switch covers the whole mechanism.

It exists because this is a fix whose ABSENCE is invisible: without it the
sweep still completes, still reports a walk that ran to `used`, and simply
throws away almost every reclaim decision it made — 40 724 of 40 746 in the
case it was written for. Two separate investigations spent rounds on symptoms
of exactly that, and neither could A/B the mechanism in one binary because
there was no way to turn it off. A cross-run before/after here prices the box,
not the change; this prices the change.

## Verification

* `TestDefaultInstanceManager`: `OK (1 test)` on the no-flag default,
  `-XX:+UseGenerationalGC`, `-XX:+UseZGC` and `-XX:+UseG1GC`; 5/5 consecutive
  runs green under Generational before any of this branch's changes, so the
  pass is not a flake being read as a fix.
* `regression-suite` core set: **43 passed, 0 failed**, including both unload
  vectors.
* `cratonvm-gc --lib` 1502 passed / 0 failed; `cratonvm-types --lib` 562 passed
  / 0 failed.

## Corrections this closes on the superseded page

* *"Also eliminated → overlay over-rooting … it cannot be the default-collector
  mechanism"* — already corrected on that page; left here only to note the
  correction is now moot, since both overlay arms (Generational 08-11, ZGC
  08-11) are fixed and the remaining symptom was neither.
* *"a third, still-unidentified **mark-phase** factor"* — it was not a
  mark-phase factor. The marker was innocent for the fourth time in a row,
  exactly as that page's own *The marker is innocent* section found; the
  `is_marked=true` reading it ended on was produced downstream of the same
  discarded-reclaims mechanism.
* *`live_instances_of_this_loader` reports 0 for all three loaders* — reads 1
  for a live loader today. Whatever the 08-11 session saw there went with the
  unwinds; no separate enumeration gap needs chasing.

## What is NOT claimed

The conservative arm still exists and still fires for spans `side_sorted` does
not vouch for and that are not a whole number of empty objects — the
double-free protection the site was written for is untouched, and a long
unlisted zeroed span (the shape that abandoned 233 MB of a 256 MB young
generation in the 08-07 report) still takes the unwind. The two are counted
apart (`SWEEP_ZERO_SPAN_EMPTY_RUNS` vs `SWEEP_ZERO_SPAN_HITS`) so the benign
shape becoming common can never read as corruption becoming common.

Nothing here raises `HEADER_SIZE`. Restoring the dead "a fresh header is never
all-zero" invariant by minting a hash eagerly or by widening the header remains
unavailable for the reason the superseded page gives — a non-zero mark word
loses the thin-lock CAS — and is not needed: the discriminators are
`side_sorted` for the live half and the on-grid empty-object-run test for the
dead half.

## Re-verified after merging `origin/dev`

`dev` moved 17 commits mid-work, touching `gc/src/gen_heap.rs` among others, so
every claim above was re-measured on the merged binary rather than carried over:

* the in-binary A/B still discriminates — recovery on: both unload vectors
  `PASS`; recovery off: `RClassUnloadSweep PASS` (default collector, still
  blind by construction) and `RClassUnloadSweepGen FAIL output differs from
  HotSpot`;
* `TestDefaultInstanceManager` `OK (1 test)` on all four collectors again;
* `regression-suite` core 43 passed / 0 failed; `cratonvm-gc --lib` 1503 and
  `cratonvm-types --lib` 562, both green.
