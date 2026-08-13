# The young walk treats a run of EMPTY objects as corruption at other sites

**Status:** **ALL EIGHT sites RESOLVED** (2026-08-13). The two that the previous
revision recorded as needing "a workload that genuinely fills the old
generation" are fixed in wave 7; that prerequisite turned out to be only half
the story, and the other half was two configuration gates and one hardcoded
argument. Both residuals have now been taken on (2026-08-13). The phantom-extent finding
is **NOT REPRODUCIBLE** — 25 runs, including at the commit it was recorded at —
so it stays open as an unexplained observation with a leading hypothesis and a
new discriminator, not as a live defect. The 16-bytes-per-empty-object retention
is **re-measured on current dev and the deferral stands**.

## Background

An empty object — `ClassId(0)`, `kind = Object`, `num_slots = 0` — is
HEADER_SIZE all-zero bytes, because `MARK_NEUTRAL`, `ObjectKind::Object` and
`ArrayElementType::Reference` are all `0`. Eight `zero_run_end` callers in
`gc/src/gen_heap.rs` shared the rule "an all-zero run of at least HEADER_SIZE at
a walk-grid offset is evidence the grid broke", and for that shape the rule is a
false positive. Four waves have now closed five of them.

## What "measured at zero" was hiding

The previous revision of this page recorded five sites at zero anomaly hits and
concluded they were inert. Two separate instrument errors were behind that, and
both are worth keeping in mind before quoting a zero:

**1. A zero anomaly count does not distinguish "ran and did not see it" from
"never ran".** `YOUNG_WALK_ENTRIES` now counts entries per walk. At the default
`--Xmx 1500m`, all five read **0 entries** — none of them execute at all. The
two inside `selective_on` need PROMOTION_AGE to be reachable, and after the
wave-1..3 fixes the workload does so few young collections (2–4 for a whole
run) that nothing ever ages into promotion. `sp_sweeps=3 sp_selective=0` says it
directly.

**2. The counter I quoted could not see the site I was quoting it about.**
`zero_spans` (`SWEEP_ZERO_SPAN_HITS`) is incremented only in the sequential main
sweep walk. The evacuation pre-pass has its own, separately-written zero-run
branch that incremented nothing. Reading `zero_spans=0` and concluding "the zero
run is not what stops the pre-pass" was reading a counter from a different
walk. This is the same failure as the ordering trap recorded in the wave-3
section: an instrument that cannot answer the question returns a confident zero.

## Reaching them, and what was there

Shrinking the heap makes selective promotion reachable. `--Xmx 320m / 450m /
700m` on `org.hibernate.reactive.BatchingConnectionTest`,
`-XX:+UseGenerationalGC`:

| | 700m | 450m | 320m |
|---|---|---|---|
| `evac_prepass` entries | 3 | 4 | 6 |
| `fixup_3a` entries | 2 | 2 | 5 |
| `mark_y2o` / `fixup_yo` / `walk_young` | 0 | 0 | 0 |
| `sp_evacuated` | 162 002 | 235 764 | 83 221 |

With the pre-pass finally running, `EVAC_UNWIND_REASONS` says what stops it —
and it is the benign shape after all:

```
evac_unwind: overshoot=0 zero_span=1..4537 bad_size=0 hole_crossing=0
             candidates_dropped=3328..162615
```

Every unwind is the zero run. **Between 3 328 and 162 615 promotion candidates
were discarded per run** — and under a permanent non-moving sweep, selective
promotion is the young generation's only exit for live data. The first revision
of this page claimed exactly this and was retracted on the faulty census above;
it was right.

## Fix

`zero_run_empty_object_resume` applied at both passes, plus the `vouched_live`
escape each was also missing. Both have `side_sorted` in scope, so the argument
is the one already validated at the three earlier sites — with a second reason
on top that is specific to these two: **an empty object has no fields**, so a
run of them holds nothing for the evacuation pass to promote or age, and nothing
for the (3a) pass to rewrite.

ABBA-interleaved, wave3/wave4/wave4/wave3, at two heap sizes, two rounds each
(16 runs, all `ok=61 failed=0`):

| | wave 3 | wave 4 |
|---|---|---|
| `evac_unwind zero_span` | 1 – 1 297 per run | **0, all 8 runs** |
| `candidates_dropped` | 3 328 – 162 615 per run | **0, all 8 runs** |

## Still open

### The three sites that no suite workload enters — ALL THREE NOW RESOLVED

All three are fixed (2026-08-13, waves 6 and 7). This section keeps what it took
to reach them, because "no workload enters this" was the wrong diagnosis twice,
in two different ways.

**The first time it was the workload.** `major=0` on every hibernate-reactive
run at every heap size from 1500m down to 190m, so neither major-path walk could
run. `probes/GcWalkProbe.java` fixed that for `mark_young_to_old_refs`.

**The second time it was not the workload at all.** Three more probe shapes
still could not reach the other two, and the reason was configuration and code,
not allocation behaviour — see below. `probes/OldGenFillProbe.java` and
`probes/HeapDumpWalkProbe.java` reach them.

#### `mark_young_to_old_refs` — fixed, and it was firing on every entry

| | entries | zero-run anomalies |
|---|---|---|
| before | 4 / 20 (two probe sizes) | **4 / 20 — 100%** |
| after | 4 / 20 (unchanged) | **0 / 0** |

ABBA-interleaved, 4 runs per arm, all `PROBE-DONE`. Every major cycle was
seeding its young→old marks from the anomaly arm's CONSERVATIVE scan of the
skipped stretch instead of from a parse.

This walk has no young live set to hand the predicate — a major cycle retains
young conservatively and deliberately walks live and dead objects alike, so
`&[]` is all there is and the no-marked-base-inside condition is vacuous. What
carries the fix instead is the second argument: **an empty object has no fields,
so a run of them contains no young→old reference to miss.** The alignment and
plausible-next-header conditions still establish that `resume` is on-grid.

#### `fixup_young_old_refs` — `System.gc()` can never reach it

The previous revision blamed the probe's `System.gc()` cadence for leaving
`sp_evacuated=0 sp_unaged=91347`. That was true and it was not the blocker.
Three gates sit in front of this walk, and only the third is about allocation:

1. **Old-gen compaction is disabled by default.** `oldgen_compact_enabled()` has
   been `CRATONVM_OLDGEN_COMPACT`-gated since 2026-08-03, pending root-cause
   attribution of the corruption it caused. `CRATONVM_GC=oldgen-compact` is the
   supported spelling; the launcher prints the migration line for the legacy
   name, which is how to confirm the flag was actually seen.
2. **An interior conservative root downgrades the cycle to the in-place sweep**
   (`COMPACT_DOWNGRADED_INTERIOR_ROOT`, negative control
   `CRATONVM_GC=-old-interior-pins`). Measured zero here, so not the blocker —
   but it is the gate to check first, because it is silent unless the counter is
   non-zero.
3. **`System.gc()` routes around the compactor entirely.** An explicit full GC
   takes the non-moving young path (`nonmoving-explicit-full-gc`), which reaches
   old gen through `sweep_old_gen_non_moving` → `old_gen_gc(.., compact = false,
   ..)` with the argument **hardcoded**. Compaction lives only on the MOVING
   path's `major_gc`, whose Phase 5 fires on `old_gen.used() >= capacity * 75/100
   || major_requested`.

So the prerequisite is not "fill old gen" but "**drive old gen past 75% by
ALLOCATION, with no `System.gc()` at all**". With `-Xmx 128m` (old = N/2 = 64 MB)
and a live set whose rotating third is replaced each round, `probes/OldGenFillProbe.java`
takes `fixup_yo` from 0 to **13–17 entries**. The tell that a run is on the wrong
path is `oldgen_coalesce: calls=N` matching the major count: that counter is
incremented by the in-place arm.

#### `walk_young_objects` — reached through the heap dump, not the marker

Two doors, and the one the previous revision chased is the shut one:

* **The concurrent old-gen marker** (`collect_young_to_old_roots`) needs
  `old_gen_needs_gc()` — old ≥ 75% — to still hold when `maybe_concurrent_gc`
  asks after a minor GC. On the generational backend it never does: the Phase-5
  major runs INSIDE the young collection and clears old first. Measured with
  `CRATONVM_DBG_MIRRORPIN=1`: `old_gen_used=130974736 / cap=134217728` (97.6%) at
  Phase 5, and 55% immediately after the major. Raising the live set to 77% of
  old capacity did not change it.
* **`hprof::dump_heap`** calls `VmHeap::walk_objects()`, which starts with
  `walk_young_objects()`, and `maybe_dump_heap_on_oom` fires it under
  `-XX:+HeapDumpOnOutOfMemoryError` with no heap-ratio gymnastics at all. That
  takes `walk_young` from 0 to **1–43 entries**.
* `jcmd <pid> GC.class_histogram` remains the third, permanently shut door:
  CratonVM implements no attach listener, so it fails with
  `java.io.IOException: non existent JVM pid`. Verified against a live probe.

One detail matters when shaping the probe: the OOM must land IMMEDIATELY after a
`System.gc()`. An explicit full GC is the one deterministic way to select the
non-moving sweep, and only that sweep leaves runs of zeroed dead empty objects in
from-space. A gradual `hog.add(new byte[chunk])` loop runs many more collections
on the way down, every one of them moving (`moving-no-jit-frames-live`), and
hands the dump a dense from-space with nothing to misread. `CRATONVM_NO_MOVING_YOUNG=1`
does **not** substitute: the decision still read `moving-no-jit-frames-live`.

#### What the fix is worth, and how that was established

On the probes the anomaly does not fire at either site: `fixup_yo` 0/13 and
`walk_young` 0/1, identical across an ABBA-interleaved pre/fix/fix/pre run. That
is a real reading and it is not the whole answer, because `walk_young_objects` is
`pub` and the regression can be driven directly instead:

```
[ plain object ][ 4 swept empty objects ][ plain object ]
```

`a_run_of_swept_empty_objects_does_not_hide_the_young_objects_after_it` walks
that from-space. **Without the fix it returns 1 of 6 objects** — the recovery arm
re-anchors at the next FREE BLOCK, so it drops the run AND the live object behind
it. With the fix it returns both plain objects.

That is why this one is a correctness fix rather than a consistency fix: the
enumeration is what `collect_young_to_old_roots` turns into the concurrent
marker's young→old roots, and its call site calls those mandatory — "a missed
mark root here = cleanup frees a live object".

### Phantom extents under memory pressure — NOT REPRODUCIBLE (2026-08-13)

The original observation, kept because it was real when taken:

| | 700m | 450m | 320m |
|---|---|---|---|
| `phantom_extents` (sequential walk) | 0 | 224 | 847 |
| `par_accepts` / `par_attempts` | 8/8 | 3/13 | 2/20 |
| chunk bails, reason | — | `phantom=12` | `phantom=66` |
| `sp_evacuated` | 162 002 | 235 764 | 83 221 |

**It does not reproduce.** Re-run against the documented recipe (batch-01 class,
`-XX:+UseGenerationalGC`, `CRATONVM_GC_STATS=1`), every run reading
`phantom_extents=0`, `live_in_dead=0`, `par_accepts == par_attempts` and every
chunk-bail reason 0:

* **6 heap sizes** — 700m, 450m, 320m, 260m, 220m, 190m.
* **12 classes** — all of batch-01 at 320m.
* **2 binaries, ABBA-interleaved** at 320m — current dev AND a build of
  `5defebbb0^`, i.e. the commit the numbers above were taken at. So this is not
  "wave 4 fixed it": it does not reproduce at its own commit either.
* The stated precondition IS satisfied — selective promotion is active in every
  run (`sp_selective=5..14`, `sp_evacuated=669..114085`), so this is not the
  "narrow probe reports its own reach" failure.

What differs is scale of work, not configuration: the original 700m run
evacuated 162 002 objects where the re-run evacuates 669. Those runs were taken
on a host at load >100 with 45 leaked Testcontainers containers, and the honest
reading is that the trigger is a timing/pressure regime that a quiet host does
not enter, not a code path that has since changed.

#### The leading hypothesis, and the discriminator that would settle it

The check's premise is "`side_sorted` holds object BASES, and live objects never
nest". The first half has one documented exception, in `mark_young`'s own
comment:

> When the anchor oracle cannot resolve a candidate ... the code below falls
> back to side-marking the RAW candidate address — which is correct only if the
> candidate happens to BE a base.

A conservative candidate is frequently an object-INTERIOR word, and the
late-resolution pass that marks the real base cannot unmark the raw address — so
`side_sorted` can carry both. An interior mark of a perfectly VALID object then
satisfies the phantom premise exactly. That fits every feature of the finding:
`live_in_dead=0` throughout (nothing was ever freed wrongly — the object is
retained, the sweep just discards its own work), the correlation with selective
promotion and with memory pressure (more fragmentation → more unresolved
candidates), and both sample reports, whose heads size CONSISTENTLY
(`144 = 16 + 16×8`, `272 = 16 + 16×16`) with the victim at an interior offset of
32 and 128:

```
offset=513936 span_bytes=144 span_head_class_id=0    kind_byte=1 num_slots=16
             victim_interior_offset=32  last_anchor_off=513296
offset=518792 span_bytes=272 span_head_class_id=65   kind_byte=0 num_slots=16
             victim_interior_offset=128 last_anchor_off=518440
```

It is a hypothesis, not a measurement, and 25 runs could not promote it. So what
landed is the **discriminator**, not a fix: `SWEEP_PHANTOM_INTERIOR_MARKS`
(`[GC] young_sweep: … phantom_nonbase_marks=`) counts how many of the marks a
reported extent subsumes are unresolved RAW candidates, and the bounded report
gained `victim_is_unresolved_raw` and `nonbase_marks_inside`. Read beside
`phantom_extents`: **equal counts say the guard is firing on interior marks;
zero says the subsumed marks were real bases and the walk really did leave the
grid.** One line, next occurrence, instead of another 25 runs.

**The verdict itself is deliberately unchanged.** Screening interior marks out
of the check is a one-line change and it is tempting, but it would weaken a
CORRUPTION guard on a symptom that cannot currently be reproduced, and it would
cost detection for a real phantom whose only subsumed mark happens to be an
unresolved candidate. That trade needs the counter's evidence first. The
end-of-sweep `dead_regions` × `side_sorted` merge remains the backstop either
way, and it RETAINS rather than zeroes.

### The 16-bytes-per-empty-object retention — MEASURED, and deliberately not closed

An accepted run is stepped over, not parsed, so each dead empty object in it
stays until a moving cycle resets from-space. Under a permanent non-moving sweep
there is no moving cycle, so the obvious worry is that this accumulates.

**It does not.** `EMPTY_RUN_BYTES_LAST` (the bytes skipped by the LAST completed
sweep — the standing retention, since every cycle re-skips the same runs; a
cumulative total would just multiply it by the cycle count) against
`young_used`, on `BatchingConnectionTest` under `-XX:+UseGenerationalGC` at
seven heap sizes:

| young collections | retained | young used | fraction |
|---|---|---|---|
| 4 (`--Xmx 1500m`) | 25 216 B | 326.9 MB | 0.008% |
| 13 (`450m`) | 28 992 B | 117.8 MB | 0.025% |
| 16 (`320m`) | 14 400 B | 83.6 MB | 0.017% |
| 20 (`260m`) | 26 720 B | 68.0 MB | 0.039% |
| 26 (`220m`) | 22 848 B | 57.4 MB | 0.040% |
| 27 (`190m`) | 22 080 B | 49.6 MB | 0.045% |

Flat at **13–29 KB from 4 collections to 27** — no trend with cycle count. The
fraction column rises only because the young generation shrinks with `-Xmx`, not
because the retained bytes grow. `CascadeComplicatedTest` at `450m`: 6 144 B.

**Why it is not being closed.** The change would be to push the run as a dead
region at the two walks that reclaim (the sequential walk and `sweep_chunk`)
instead of stepping over it. The liveness argument is actually sound — marks
live in the side channel keyed by ADDRESS, so a live object whose header was
clobbered to zero is still in `side_sorted`, and the predicate already refuses
any run containing a marked base; an unmarked slot is what the sweep frees
everywhere else. But it would be **the first change in this family that frees
something the previous code retained.** Waves 1–4 only ever reduced how much the
walk skipped, which is safe in the direction the collector already errs. Trading
that property for ≤0.045% of the young generation is not a good trade, and the
retained span doubles as a margin against precisely the hazard the original
comment named — a live allocation whose header a stale register-held reference
clobbered, a family that has cost this codebase several investigations.

The instrument is kept so the decision is re-checkable rather than a remembered
opinion: if a workload ever shows this figure growing with cycle count, that is
new evidence and the trade changes.

**Re-checked 2026-08-13, after waves 5–7.** Same instrument, same class
(`BatchingConnectionTest`, `-XX:+UseGenerationalGC`), current dev — the point
being that waves 6 and 7 added two more sites that step over empty-object runs,
so the standing retention could have grown:

| `-Xmx` | retained | young used | fraction |
|---|---|---|---|
| 700m | 21 728 B | 96.2 MB | 0.023% |
| 450m | 11 520 B | 96.3 MB | 0.012% |
| 320m | 19 072 B | 52.6 MB | 0.036% |
| 260m | 20 768 B | 68.2 MB | 0.030% |
| 220m | 19 040 B | 57.4 MB | 0.033% |
| 190m | 20 256 B | 49.7 MB | 0.041% |

11.5–21.7 KB against 13–29 KB before, and the fraction ceiling is 0.041% against
0.045%. No growth, and the two new sites did not move it — which is what the
mechanism predicts, since both of them run on paths that reclaim nothing
(`fixup_young_old_refs` walks a freshly-compacted to-space, and
`walk_young_objects` only enumerates). **The deferral stands, on fresh numbers
rather than on the earlier decision being remembered.**
