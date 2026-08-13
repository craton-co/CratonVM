# The young walk treats a run of EMPTY objects as corruption at other sites

**Status:** the two sites that can be reached from a real workload are
**RESOLVED** (2026-08-13). **Three remain**, and the reason they remain is now
precise: they are never entered at all on any workload reachable from this
repro, so no change to them can be measured. A separate finding — phantom
extents under memory pressure — is split out at the bottom.

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

### Three sites that cannot be reached

`mark_young_to_old_refs`, `fixup_young_old_refs` and `walk_young_objects` read
**0 entries** at every heap size tried. The first two are on the major-GC /
compaction path (`sp_defrag=0` throughout) and the third is the diagnostic walk
behind `jcmd GC.heap_info` / `GC.class_histogram`. Wiring them would be an
unmeasurable change, and unlike the two just fixed, none of them has a live set
to pass the predicate — `mark_young_to_old_refs` has `walked_bases` (the bases
the sweep verified, arguably a better on-grid oracle), `fixup_young_old_refs`
has nothing, `walk_young_objects` has nothing. Whoever needs them should first
find a workload that enters them; `YOUNG_WALK_ENTRIES` makes that a one-line
check rather than a guess.

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

### The 16-bytes-per-empty-object retention

Unchanged from wave 3: an accepted run is stepped over, not parsed, so each dead
empty object stays until a moving cycle resets from-space. Bounded and
self-limiting; parsing them is a separate safety argument.
