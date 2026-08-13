# The young walk treats a run of EMPTY objects as corruption at other sites

**Status:** all SIX sites that any workload has been made to enter are
**RESOLVED** (2026-08-13). Two remain, and the reason is now precise rather than
assumed: both need a workload that genuinely fills the old generation, which
three probe shapes did not manage. Two other residuals are recorded at the bottom — a NEW and
unexplained phantom-extent finding under memory pressure, and the
16-bytes-per-empty-object retention, which is now measured and deliberately left
alone.

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

### The three sites that no suite workload enters

`mark_young_to_old_refs` is **fixed** (2026-08-13). The other two are still
unreached, and this section records exactly what it took to find that out so the
next attempt starts from evidence rather than from a fresh guess.

**All three are entered only from paths a suite workload never takes.** `major=0`
on every hibernate-reactive run at every heap size from 1500m down to 190m — no
major GC ever happens, so neither major-path walk can run, and
`walk_young_objects` is diagnostic. `probes/GcWalkProbe.java` reaches the first
of them: it churns `new Object()` (the all-zero-header shape), keeps a chained
surviving set so there are real young→old edges, and calls `System.gc()`, which
CratonVM turns into an explicit `request_major_gc()` rather than waiting for old
gen to reach 75%.

#### `mark_young_to_old_refs` — fixed, and it was firing on every entry

With the probe reaching it, the anomaly is not occasional:

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

#### `fixup_young_old_refs` — needs a compaction that MOVES something

Gated on `!compact_map.is_empty()`. Three probe shapes produced 10–20 major GCs
and never a non-empty map: `sp_evacuated=0 sp_unaged=91347` says why — the
probe's `System.gc()` cadence collects objects before they reach PROMOTION_AGE,
so old gen never accumulates enough to have anything to slide.
`probes/OldGenProbe.java` is the closest attempt (long-lived blobs with bulk
payload, scattered thirds dropped each round to fragment rather than truncate);
it needs a shape that lets objects AGE first — fewer, later `System.gc()` calls,
or an old generation small enough to hit the 75% trigger on its own.

#### `walk_young_objects` — not reachable by `jcmd` on this VM

Its callers are `walk_objects()` (heap dump / class histogram) and
`collect_young_to_old_roots()` (the CONCURRENT old-gen marker's `initial_mark`
and `remark`). Neither is reachable today:

* `jcmd <pid> GC.class_histogram` fails with
  `java.io.IOException: non existent JVM pid` — CratonVM does not implement the
  HotSpot attach listener, so no `jcmd` command reaches it. Verified against a
  live probe process, not inferred.
* the concurrent marker is gated on `old_gen_needs_gc()`, which is the same
  old-gen-pressure condition `fixup_young_old_refs` needs.

So both remaining sites reduce to one prerequisite: **a workload that genuinely
fills the old generation.** `YOUNG_WALK_ENTRIES` turns "did I manage it" into a
one-line check.

### Phantom extents under memory pressure — NEW, unexplained

Found while reaching for the sites above, and not part of this family:

| | 700m | 450m | 320m |
|---|---|---|---|
| `phantom_extents` (sequential walk) | 0 | 224 | 847 |
| `par_accepts` / `par_attempts` | 8/8 | 3/13 | 2/20 |
| chunk bails, reason | — | `phantom=12` | `phantom=66` |

Every bail is reason `phantom` — a header whose extent subsumes a marked object
base, which is the corruption family, not a benign shape (`zero_span=0` and all
three zero-run refusals 0 at every size). It appears only when selective
promotion is active, which makes forwarded headers the obvious suspect —
**ruled out**: `ObjectHeader::make_forwarded` is
`quartet_of(prev) | target | MARK_FORWARDED`, so kind, element type, `gc_age`
and `gc_flags` all survive forwarding and `gen_object_total_size` sizes a
forwarded header correctly. Sample reports:

```
offset=513936 span_bytes=144 span_head_class_id=0    kind_byte=1 num_slots=16
             victim_interior_offset=32  last_anchor_off=513296
offset=518792 span_bytes=272 span_head_class_id=65   kind_byte=0 num_slots=16
             victim_interior_offset=128 last_anchor_off=518440
```

`live_in_dead=0` throughout, so the guard is catching it and re-anchoring before
anything is freed — it is a throughput cost and a grid-integrity signal, not a
known reclamation bug. Repro: any batch-01 class at `--Xmx 320m` under
`-XX:+UseGenerationalGC` with `CRATONVM_GC_STATS=1`.

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
