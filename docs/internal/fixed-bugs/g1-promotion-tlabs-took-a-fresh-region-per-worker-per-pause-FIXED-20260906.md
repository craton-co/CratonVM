# G1: every evacuation worker's promotion TLAB took a fresh region each pause, so the old generation grew by the WORKER COUNT per young pause

| | |
|---|---|
| **Status** | **FIXED** (`CRATONVM_G1_PARALLEL_EVAC_RESUME_DEST`, default ON). The instrument that hid it — the per-pause region census, filled by the serial driver only — is fixed on the other three drivers too. |
| **Symptom** | `OutOfMemoryError: Java heap space` at `-Xmx2g` on `org.apache.catalina.startup.TestHostConfigAutomaticDeploymentXmlExternalWarXml`, a workload **HotSpot runs to completion in 18 s at `-Xmx512m` with an 18-24 MB live set**. |
| **Left behind by** | `g1-eight-byte-write-at-a-live-objects-base-FIXED-20260908`, whose closing line — "the remaining failures are genuine heap exhaustion at `-Xmx2g`" — was an assumption, not a measurement, and is wrong. |

## The baseline that makes this a defect rather than a workload

HotSpot 25, same classpath, same eight test cases, `-Xlog:gc`:

```
-Xmx512m : PASS in 18 s, 11 young pauses, every one 316-321M -> 18-24M
-Xmx2g   : PASS in 25 s
```

The live set is **flat at ~20 MB across all eight deployments** and total
allocation for the class is ~2.9 GB. Nothing about this workload needs two
gigabytes. CratonVM exhausts them.

## What the heap actually does

`--verbose:gc`, one default run to the OOM (2048 regions of 1 MiB):

```
pause  type       freedMB copiedMB  free  eden  surv   old  cset
   17  YoungOnly       59      4.5  1923     9    38    78    80
   18  YoungOnly       63      7.2  1903     7    37   101    85
   19  YoungOnly       62      5.7  1881     7    36   124    85
   20  YoungOnly       62      4.5  1859     6    36   147    85
   ...
  165  YoungOnly        0      1.3     1    10    24  2013    10
```

`old_regions` climbs **monotonically, by exactly 23 per pause**, from 0 to
2026-2034 of 2048. `free_regions` reaches 0. From there every pause copies
nothing and frees nothing, and the VM throws `OutOfMemoryError`.

**Twenty-three is the evacuation worker count.** `ergonomic_gc_worker_threads`
gives `8 + (32-8)*5/8 = 23` on this 32-CPU box. One binary, arms interleaved:

| arm | modal `old_regions` growth per pause | peak Old | min free | verdict |
|---|---:|---:|---:|---|
| default (23 workers) | **+23** (58 of 1192 pauses) | 2026 | 1 | OOM |
| `CRATONVM_G1_WORKERS=4` | **+4** (64 of 85 pauses) | 297 | 1715 | PASS |
| `CRATONVM_G1_PARALLEL_EVAC=0` | +1 | **30** | 1979 | PASS |

The growth quantum IS the worker count, and it is what decides the outcome.

## The mechanism

`SharedEvac::tlab_alloc` can only claim from `pool` — the **Free** regions
reserved before the dispatch:

```rust
let i = self.pool_next.fetch_add(1, Ordering::Relaxed);
...
let region = &mut *self.regions_base.0.add(idx);
region.region_type = tlab.dest_type;   // Free -> Old
tlab.offset = 0;
```

`SharedEvac` is built per pause and `retire_all` clears both TLABs at the end
of it, so every worker starts each pause with `region_idx: None` and claims a
**whole fresh region** for its Old destination. It then fills a fraction of it
— the run above copies 1.6-8 MB per pause in total, spread over 23 regions —
and abandons it. Nothing ever returns to that region: the next pause's pool is
built from `RegionType::Free`, and a partly-filled Old region is not Free.

So the old generation grows at `workers` regions per young pause **whatever the
promoted volume is**. Over the whole run at most 0.64 GB was ever copied, into
2.03 GB of Old regions.

The serial evacuator never had the defect. `alloc_in_type_locked` tries a hint
region and then `alloc_in_type_locked_scan` scans **every existing non-CSet
region of the destination type** before claiming a Free one. That is the whole
difference between 30 Old regions and 2026, and it is why
`CRATONVM_G1_PARALLEL_EVAC=0` has been the kill switch that makes this class
healthy — a fact the WarXml page recorded (parallel 0/10 healthy, serial 5/5)
and could not explain.

## What made it fatal rather than merely wasteful

**Nothing reclaims the old generation until after the OOM has been thrown.**
Across 63-, 201-, 342- and 1192-pause runs, every single pause is `YoungOnly`
and `[GC] g1 cycle` reports `cset_old=0`. The first `Mixed` pause in every run
appears *after* the `OutOfMemoryError`, on the `last_ditch_reclaim` path — and
when it finally runs it hands back **1100 regions at once, then another 204**:

```
java.lang.OutOfMemoryError: Java heap space          <- the OOM
[GC-STAT] type=Mixed ... free_regions=1100 old_regions=935
[GC-STAT] type=Mixed ... free_regions=1304 old_regions=731
```

1.3 GB of the 2 GB was garbage the whole time. The mixed collector works; it is
never asked. `maybe_concurrent_gc` — the only caller of either half of the mark
lifecycle — hangs off `maybe_gc`'s epilogue, which G1 does not reach because it
triggers its own young pauses from inside the allocator (see
`reference_a_dead_lifecycle_hides_every_defect_in_the_path_it_gates`, measured
on H2 the day before). `CRATONVM_G1_JIT_MARK_DRIVER=1` does not change the
outcome here: its first mixed pause is still the post-OOM one, because IHOP is
compared against **bytes live in Old regions**, and Old regions holding 0.64 GB
in 2.03 GB of space read as ~31% of a 70% threshold they can never cross.

That is two independent brakes failing in series. This page fixes the first
one, which is the one that fills the heap; the second is
`CRATONVM_G1_IHOP_COUNTS_REGIONS`, still opt-in, whose own doc records why.

## The fix

`CRATONVM_G1_PARALLEL_EVAC_RESUME_DEST` (default ON). Each pause builds a
`resume` list of non-CSet Old regions that still have room, and a promotion
TLAB takes one up — **resuming at that region's existing cursor** — before it
consumes a fresh Free region:

```rust
if self.claim_resume_region(tlab, size) { continue; }
let i = self.pool_next.fetch_add(1, Ordering::Relaxed);
```

The claim is a `fetch_add` into `resume`, so an index goes to exactly one
worker and the `&mut G1Region` never aliases — the same discipline the
Free-pool claim uses. `retire_tlab` stores `tlab.offset` as the region's
cursor, so starting at the old cursor round-trips exactly. `resume_set` joins
`pool_set` in the exclusion `shared_dest_alloc` applies, because a resumed
region is a TLAB's and that cursor store would discard anything bumped into it.

Old only: on a young pause the CSet takes every Eden and Survivor region, so a
Survivor resume list is empty by construction; on a mixed pause the CSet term
removes the Old regions being collected.

### What it is worth

One binary, `CRATONVM_G1_PARALLEL_EVAC_RESUME_DEST=0` against the default, arms
interleaved within each repetition.

At `-Xmx2g` (2048 regions) the growth rate is the whole story — the runs are
the same length and copy the same bytes, and only the region consumption
differs:

| arm | pauses | bytes copied | modal Old growth | **peak Old** | min free |
|---|---:|---:|---:|---:|---:|
| off | 47 | 0.49 GB | **+23** | 722 | 1299 |
| off | 74 | 0.68 GB | **+23** | **1148** | 873 |
| on | 79 | 0.69 GB | **+1** | **50** | 1948 |
| on | 73 | 0.74 GB | **+1** | **49** | 1953 |

At `-Xmx2g` both arms can still finish before the heap is gone, so the outcome
is a race and a 2 GB census is under-powered for it. At `-Xmx1g` (1024 regions,
a heap HotSpot does the same work in with 512 MB) it is not:

| arm | pauses | **peak Old** | min free | verdict |
|---|---:|---:|---:|---|
| off | 114 | **1018** of 1024 | **0** | OOM |
| off | 169 | **999** | 1 | OOM |
| off | 2415 | 440 | **0** | OOM (in the post-exhaustion regime, where nothing moves and the growth quantum no longer shows) |
| on | 191 | **59** | 921 | PASS |
| on | 313 | **79** | 896 | all 8 tests ran |

**The old generation stops tracking the worker count and starts tracking the
promoted volume**, which is what it was always supposed to do.

The second `on` run at 1 GB is recorded as "all 8 tests ran" rather than PASS
because it died in `JUnitCore.removeListener` with `this.notifier` null after
the eighth test — a field read that returned null on a live object, i.e. the
corrupt-cell family the WarXml page is about. That defect is still open and is
not this one.

### Re-measured on the merged dev tip

The tables above were produced on this branch's own binary. `dev` moved 475
lines of `g1.rs` under it in the same day — including the evacuator's
forwarding-tag retirement — so the same A/B was repeated on the merge result
(`e5202bef6`), one binary, arms interleaved, `-Xmx2g`:

| arm | pauses | **peak Old** | min free | modal Old growth | `resumed_dest_regions` | verdict |
|---|---:|---:|---:|---:|---:|---|
| off | 51 | 649 | 1351 | **+23** | — | PASS |
| off | 58 | **953** | 1032 | **+23** | — | PASS |
| on | 81 | **46** | 1955 | **+1** | 1296 | PASS |
| on | 45 | **42** | 1950 | **+1** | 610 | PASS |

**At `-Xmx1g` the class is NOT fixed, and neither arm is healthy on that tip.**
One run each: `off` OOMed with Old at 1021 of 1024; `on` kept Old to 43 and
still FAILed 6 of 8 cases with `Error starting child` and a cascade of
`FileAlreadyExistsException: external.war` from the first case that failed to
tear down — a functional failure at `free_regions=0`, not an old-generation
one. What this fix removes is the old generation eating the heap; the rest of
what that class does under pressure is the WarXml page's business.

## What made it invisible

**The per-pause region census was filled by `young_collection_serial` alone.**
`G1PausePhases::record_region_census` had one call site, so on the DEFAULT
(parallel) arm every `[GC-STAT]` line printed

```
free_regions=0 eden_regions=0 surv_regions=0 old_regions=0 hum_regions=0 cset_regions=0
```

— 63 pauses of it, an all-zero census that reads as a fact about the heap and
is a fact about the instrument. The field's own doc describes the *previous*
incarnation of this bug ("An instrument armed where it cannot fire"): the fix
for that one landed on one of the two arms. Fixed here on
`young_collection_parallel`, `mixed_collection` and `mixed_collection_parallel`,
with `both_young_arms_report_a_region_census` driving each arm DIRECTLY rather
than through the dispatcher, because a test that goes through the dispatcher
exercises whichever arm the ambient flags select — which is exactly the defect
such a test cannot see.

**And one diagnostic that lies.** `CRATONVM_G1_DBG_REACH=1` reads like the
occupancy-census switch — it is what adds `old_bytes=` to `[GC-STAT]` — but it
also arms `dbg_verify_reachable_integrity`, a whole-reachable-heap walk on
every pause. With it on, `verify_us` was 263 ms of a 292 ms pause, the adaptive
young target collapsed, and the run went to 1384 pauses each freeing about a
kilobyte. That is a different collector, not a measurement of this one.

## Reproduction

```
cratonvm.exe --java-home <jdk25> --Xmx 2g -XX:+UseG1GC --verbose:gc ... \
  -c <jars-first classpath> org.junit.runner.JUnitCore \
  org.apache.catalina.startup.TestHostConfigAutomaticDeploymentXmlExternalWarXml
```

Read `old_regions` in the `[GC-STAT]` lines. It should stop growing once the
application's live old set is reached; a run in which it grows by the same
number every pause, and that number is `[GC] g1 young evacuation: workers_last`,
is this defect. `[GC] g1 promo_dest: resumed_dest_regions=` counts the fix
engaging.
