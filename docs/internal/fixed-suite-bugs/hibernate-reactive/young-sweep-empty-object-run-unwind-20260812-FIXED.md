# Young non-moving sweep unwound every reclaim decision for a dead `new Object()` — `BatchingConnectionTest` hang — FIXED

**Status:** FIXED (2026-08-12), `gc/src/gen_heap.rs`. Found working
`docs/known-issues/hibernate-reactive/investigate-batch-01.md` — the only
CratonVM defect left in that 12-class batch once the SASL/SCRAM and JNA
blockers it was collected behind had been fixed.

**Shape:** `org.hibernate.reactive.BatchingConnectionTest` HUNG (rc=124) under
`-XX:+UseGenerationalGC`. It passes under G1, under ZGC, under the no-flag
default (ZGC), under `--nojit`, and on HotSpot JDK 25. Not a Hibernate-Reactive
bug, not a Postgres bug, and nothing to do with batching: a young-generation GC
livelock that any allocation-heavy workload can hit whenever the generational
collector is selected AND the JIT has compiled anything.

## What it looked like

```
@@TESTFAIL org.hibernate.reactive.BatchingConnectionTest testBatching(VertxTestContext) FAILED
java.util.concurrent.TimeoutException: testBatching(io.vertx.junit5.VertxTestContext) timed out after 120 seconds
```

— the FIRST test method, then `after()` timing out too, then the process wedged
until the harness cap. Under the same binary with `-XX:+UseZGC` the whole class
runs in 10.6 s; on HotSpot, 5.4 s.

Two things were visible in the log and both were misleading on their own:

```
[moving-young] fallback #4096: reason=unregistered-jit-frame-on-stack
young non-moving sweep: the header at this offset claims an extent that
  SUBSUMES a live (marked) object ... span_head_class_id=0 kind_byte=1
```

The `[moving-young] fallback` line is **normal**: while
`JIT_PUBLISHES_RELOCATION_CONTRACT` is false, a live compiled frame forces the
non-moving sweep for the whole process, so on any JIT-warm workload *every*
young cycle takes it. It says nothing about this bug except that the sweep below
is the one that runs. Setting `CRATONVM_XT_HELPER_WINDOW_SCAN=0` (the other
fallback reason in the log) did not help — the fallback simply reverts to
`unregistered-jit-frame-on-stack`.

The phantom-extent guard fired 8 times, which is its report cap, not its count.
It was a red herring: the per-cycle census below recorded `phantoms=0` for every
cycle that mattered.

## Diagnosis

`CRATONVM_DBG_YOUNG_TRIGGER=1` named the regime in one line:

```
[young-trigger] n=176128 cursor=192MB free_list=0MB live=192MB threshold=187MB non_moving=true
[young-trigger] n=196608 cursor=340MB free_list=2MB live=337MB threshold=187MB non_moving=true
```

`free_list` pinned at ~0 while the bump cursor climbs past the trigger is the
livelock: `needs_gc` is true on essentially every allocation, the collector runs
continuously, and it reclaims nothing.

`CRATONVM_DBG_SWEEP_CENSUS=1` said what "nothing" meant. Early cycles reclaimed
thousands of objects; from cycle 19 — i.e. once the JIT had compiled enough that
the non-moving sweep became permanent — **every cycle reclaimed exactly one
object, and it was always a TLAB filler** (`cid0xf111e700`).

A temporary per-cycle census in the sweep found the mechanism:

```
[SWEEPTOT] cycle=0 used=196814776 free_list=789096 objects_live=195503
  objects_swept=293 bytes_swept=789096 dead_regions=22
  unwinds=147 sites=[0, 147, 0, 0] unwound_entries=40724 phantoms=0
  zs_hist=[147, 0, 0, 0, 0] zs_bytes=2352 zs_on_anchor=0
```

The walk **did** find the garbage — 40 746 dead regions — and then threw 40 724
of them away. All 147 unwinds came from one site, the unlisted-all-zero-span
recovery, and `zs_hist` (a run-length histogram over `[16, 32, 48, 64, >64]`)
says every one of those 147 spans was **exactly 16 bytes**: 2 352 bytes in
total, costing the entire sweep. No other desync guard fired on those cycles —
phantom extents 0, implausible sizes 0, free-hole overlaps 0.

## Root cause

An empty object — `ClassId(0)`, `kind = Object` (tag 0), `num_slots = 0`,
identity hash not yet minted — is HEADER_SIZE all-zero bytes, and
`gen_object_total_size` sizes it at exactly HEADER_SIZE. That has been an
ordinary, common allocation ever since HEADER_SIZE shrank 24 → 16 made the mark
word the second header word and the JIT's `new Object()` fast path left it zero
(`jit/src/x64/objects.rs`; the interpreter mints the hash eagerly, which is why
`--nojit` never saw this).

A **dead** one is unmarked, so the sweep's own live set (`side_sorted`) cannot
vouch for it, and it fell into the walk's unlisted-zero-span anomaly path. That
path unwinds every reclaim decision taken since the last grid anchor, on the
grounds that a mis-sized stride landing in a live object's zeroed interior
produces the same signature.

This is the residual of the retired
`jit-young-heap-exhaustion-after-header-16-FIXED-20260807` write-up. That fix
corrected the **resume** — re-anchor at an allocator-recorded object start
instead of the next free block, which is what had abandoned 233 MB of a 256 MB
young generation — but kept the **unwind**. With the resume fixed the unwind is
cheap per event and catastrophic per cycle: 147 events × ~277 discarded
decisions each.

## Fix

`gc/src/gen_heap.rs`: a new `zero_run_is_empty_object_run` predicate, applied at
the sequential young sweep walk. A zero run is a run of empty objects, not a
desync, when all three hold:

1. the run is a whole number of HEADER_SIZE object slots;
2. no marked object BASE starts strictly inside it (one AT the run start is the
   pre-existing `vouched_live` case, which parses normally instead);
3. the header at `run_end` itself sizes plausibly, or the run ends the arena.

Those three are exactly what makes resuming at `run_end` on-grid. The run is
still **never parsed and never freed** — over-retention is always safe here, and
the span may equally be a live allocation whose header a stale register-held
reference clobbered — so this cannot reclaim anything the old code retained. It
only stops discarding the decisions behind it. Anything else, notably a long
unlisted zeroed span, still takes the unwind, and the two are counted apart
(`SWEEP_ZERO_SPAN_EMPTY_RUNS` vs `SWEEP_ZERO_SPAN_HITS`) so "the benign shape
became common" can never be read as "the corruption became common".

## Measurement

Same binary, same class, `-XX:+UseGenerationalGC`, Azure host
`azureuser@20.80.105.49`:

| | before | after |
|---|---|---|
| young collections in 180 s | 620+ | 2–4 for the whole run |
| `bytes_swept` per cycle | 262 KB – 789 KB | ~175 MB |
| `dead_regions` surviving per cycle | 1–22 | 1 337 – 42 413 |
| `unwound_entries` per cycle | 40 724 | 0 |
| class result | HANG (rc=124) or FAIL in 4 of 6 runs | PASS 61/61 in 6 of 6 runs, 17.5–20 s |

Batch-01's 12 classes, one class per process, against this fix:

| GC | result |
|---|---|
| `-XX:+UseGenerationalGC` | 12/12 PASS (three independent batches) |
| `-XX:+UseG1GC` | 12/12 PASS |
| `-XX:+UseZGC` | 12/12 PASS |
| no GC flag (default) | 12/12 PASS |
| HotSpot JDK 25 oracle | 12/12 PASS, same found/ok/skipped counts |

`cargo test -p cratonvm-gc`: 1488 passed, 0 failed (1481 before, +7 new).

## Regression test

`gen_heap::tests::{one_empty_object_of_zeros_is_not_a_desync,
several_empty_objects_of_zeros_are_not_a_desync,
a_misaligned_zero_run_is_still_a_desync,
a_live_base_inside_the_run_is_still_a_desync,
a_live_base_before_the_run_does_not_veto,
a_zero_run_to_the_end_of_used_is_not_a_desync,
an_implausible_header_after_the_run_is_still_a_desync}`.

Mutation-checked: disabling all three guards at once kills exactly the three
"is still a desync" tests and leaves the four positive ones passing, so each
guard is load-bearing and no test passes for the wrong reason.

## Residual — the same predicate is wrong at other walk sites

Filed as
`docs/known-issues/hibernate-reactive/young-walk-zero-run-sibling-sites.md`.
Seven other `zero_run_end` callers in `gc/src/gen_heap.rs` treat the identical
benign shape as corruption; two of them are measurable on this same repro
(`sweep_chunk` aborts the parallel prefix, `clear_all_mark_bits_in_arena`
leaves stale mark bits behind). They are throughput and over-retention costs,
not the hang, and are deliberately out of this change.

---

## Wave 2 (2026-08-12, same day): the parallel sweep and the mark-clearing walk

The residual filed above was worked immediately. A per-site census on the same
repro settled which of the seven siblings actually fire — and it did not agree
with the filing:

| site | anomaly hits over 4 young cycles |
|---|---|
| `sweep_chunk` (parallel prefix walker) | 5, and `par_attempts=5 par_fails=5` |
| `clear_all_mark_bits_in_arena` | 2 498 |
| selective-promotion evacuation pre-pass | **0** |
| second pre-pass walk | **0** |
| `mark_young_to_old_refs` | **0** |
| `fixup_young_old_refs` | **0** |
| `walk_young_objects` | **0** |

So the filing's claim that selective promotion was being suppressed was wrong —
a plausible reading of identical code that the measurement does not support.

The two that fire are now fixed with the same predicate plus the `vouched_live`
escape each was also missing. The `clear_all_mark_bits_in_arena` one is the more
interesting of the two: re-anchoring there **leaves the mark bits in the skipped
stretch set**, and the next non-moving sweep treats a set `GC_FLAG_MARKED` as
live regardless of reachability, so the false positive fed itself. It now takes
the live set from its single caller, which had it in hand all along.

**`PAR_SWEEP_ATTEMPTS` / `PAR_SWEEP_ACCEPTS` have existed since H2-CID0 with a
doc comment saying "a large abort count means chunks routinely disagree with the
grid" — and no reader.** They now print from `print_gc_summary` under
`--verbose:gc` / `CRATONVM_GC_STATS`, which is how the before/after was taken:

| ABBA, 8 runs per arm | before | after |
|---|---|---|
| parallel sweep accepted | 0 of 26 attempts | 17 of 23 |
| `phantom_extents` / `live_in_dead` | 0 / 0 | 0 / 0 |
| wall clock (median) | 10.94 s | 10.83 s |

The wall-clock columns are indistinguishable: this restores a path that was 100%
dead on every JIT-warm workload, it does not measurably speed this class up on
this host. Both corruption guards stayed silent with the parallel path live.

Batch-01 re-validated on the new binary: 12/12 under Generational, G1 and ZGC;
under the default 10/12 in-batch with both failures re-run 3/3 green after
sweeping 8 leaked Testcontainers Postgres containers — the
`HR000092: Unable to open a connection` / `ClosedConnectionException` shape,
which is host pressure and not a VM defect. `cargo test -p cratonvm-gc` 1488/0.
A 60-class h2database slice: PASS=50, same as both earlier arms, with the only
deviations from `baseline.tsv` inside the already-failing set.

The five zero rows are left alone deliberately, and the remaining 26% of
parallel aborts is the open residual — both in
`docs/known-issues/hibernate-reactive/young-walk-zero-run-sibling-sites.md`.
