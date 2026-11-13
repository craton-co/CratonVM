# G1: young evacuation copies about 3x slower per byte than HotSpot, and `KeepLong` runs about 7x slower

## Round g1sz (2026-10-26/27)

Host 64.177.43.150 (16 cores, shared; load average per row below). Fat-LTO builds of branch g1sz; HotSpot
25.0.4 with `-XX:+UseG1GC -Xmx1g`. Interleaved runs, median of 3 per arm (`kabn.sh`):

| binary | wall CratonVM / HotSpot | CPU CratonVM / HotSpot | young pause sum | full pause sum | serial young pauses |
|---|---|---|---|---|---|
| s5 (round start) | 3.11x | 1.98x | 17.2 s (HotSpot 5.7) | 5.0 s (0.9) | 4 |
| s6 (the final defaults, before the presorted in-place map) | 2.42x | 1.83x | 12.3 s (5.7) | 3.8 s (0.9) | 0 |
| s7 (the final branch, `169c240672`), host load 3.5 | **2.44x** (7.0 s against 2.9 s) | 2.05x | 3.0 s (1.1) | 0.97 s (0.22) | 0 |
| s9 (the branch merged with `dev`, `00832538c5`), load 2.4 | 2.59x; **2.39x** with `CRATONVM_G1_PINS_PEER_STACK_WORDS=0` (s7 alongside: 2.39x) | 1.98x | 2.8 s (1.1) | 1.27 s (0.22); 1.06 s with the pins off | 0 |
| g1pin p6 (object pins, `3772f3d40e`), load 16-25 | 2.67x; 2.52x with the pins off | 1.86x; 1.76x | 7.6 s (3.3); 7.4 s | 3.3 s (0.6); 2.6 s | 0 |

The s9 row's extra 0.2x is `dev`'s peer-stack region pins (`c4ecb17ec2`), filed as
`g1-peer-stack-word-pins-hold-eden-regions-and-double-young-copies-20261027`
and fixed by g1pin (2026-10-27): a blocked peer's word now pins its object, not its region. The g1pin
row, at a much higher load, keeps 1.06x of the no-pin wall (against s9's 1.08x); what is left of it is
in the full collections, whose in-place compaction keeps the region pin.
The s5 and s6 rows ran at load 45-55; repeats of the s6 arm against HotSpot at other times read
2.44x / 1.88x, 2.83x / 1.81x and 3.04x / 1.96x. The s7 row ran on a nearly idle host and still reads
2.44x: without contention the gap is the collector's own (young 2.7x, full 4.4x HotSpot's), and under
load it widens because CratonVM asks for twice the CPU (`schedstat`: CratonVM's threads ran 39 s and waited 161 s on the run queue in one run; HotSpot's ran
21 s and waited 89 s).

**What landed** (each with its switch; details on the sizing page's "What changed"): Phase 4 skips the
regions the closure settled; the forwarding map is built sorted by a parallel bucket sort; long keep-alive
lists are seeded on the pool; young pauses stay parallel under the allocation latch and on a Free pool
short of the young set (`CRATONVM_G1_PARALLEL_UNDER_SHORT_FREE`: the last young pause of each round ran
serial at 480-535 ms of closure, against 52-99 ms parallel); one candidate classification instead of two,
the implausible-header screen inlined; batched provenance records; a lock-free metadata-pin key filter;
the in-place full compaction carries sizes per pair, resolves marks through a start bitmap, and takes its
plan's pairs as the sorted map.

**Where a `G1epKeepRounds 10` run goes now** (s6, two runs at 26.8 / 25.9 s against HotSpot's 9.8 / 10.2):

| | CratonVM | HotSpot |
|---|---|---|
| young pauses | 79-81, 11.0-11.8 s, 8.6 GB copied | 68, 4.7-4.8 s, about 7.3 GB copied (region transitions) |
| full collections | 9, 3.4-3.5 s | 9, 0.71-0.79 s |
| in-place phases, 9 fulls: parse / mark / plan / map / copy / fix-up | 0.74-0.89 / 0.81-0.84 / 0.47-0.54 / 0.33-0.42 / 0.35-0.36 / 0.52-0.56 s | (mark, prepare, adjust, compact) |
| take-over post-quota (`xt_post_quota_us`, outside `collect_us`) | 1.5-2.5 s | - |
| mutator with no collection (`-Xmx12g`, run ms) | 5.3-6.0 s | 3.5-3.7 s |
| boot and exit | about 1.9 s | about 1.1 s |

**What is left, by size:**

1. **The young copy's CPU per object**, about 1.7x HotSpot's. A `perf` profile puts it in the copy and
   the claim (`evacuate_screened` 19%), the candidate's first header miss (`classify_candidate_header_view`
   + `evacuation_candidate_screen_view` 10%), and the forwarding map's build (sort, scatter, index: about
   6%). Measured and rejected: the screens off (`CRATONVM_G1_PARALLEL_EVAC_SCREEN=0`, CPU 39.5 against
   40.0 s: the miss moves into the copy), prefetching referents (`CRATONVM_G1_EVAC_PREFETCH`, now opt-in),
   fewer workers (8 and 5: slower at the same CPU), `CRATONVM_G1_EVAC_LEAF_NO_PUSH`. HotSpot keeps
   forwards in headers only; the forwarding map exists because the VM's root remap reads
   `GcResult::pointer_map`.
2. **The full collection**, 4.5x HotSpot's. It makes six passes where HotSpot makes four: a parse of
   every object, dead ones included (0.8 s), before the mark, and a separate fix-up walk after the
   copy. A mark into a bitmap that needs no parse, and forwarding resolved from the plan, are the two
   shapes.
3. **The concurrent mark traces through objects above TAMS.** The gray set sits at its ~1.05 M cap
   with 0.05-0.5 M young entries, and every young pause during a cycle seeds them as keep-alive roots,
   re-grays their copies and remaps the whole set: 1.2 s of serial driver time over 41 pauses in one run
   (the split 0.39 s, the re-gray and remap tail 0.82 s; `CRATONVM_G1_KEEPALIVE_REGRAY_ONCE=1` halves
   the tail but lengthens the cycle, neutral overall). HotSpot
   never marks above TAMS. Not marking there would make the mark's completeness rest on every store path's
   SATB pre-barrier, so it is a correctness change, not a tuning one.
4. **Post-quota take-over time** (`vm/src/runtime/interpreter/gc_and_alloc.rs` `stw_take_over_and_wait`):
   1-25 ms per pause in the helper-window block, which finds no window on this row
   (`helper_windows=0` on all 109 pauses of one run). `CRATONVM_XT_HELPER_UNPARKED_READ=1` does not move
   it. Owned by the JIT root scan, not the collector.
5. **The mutator**: the main thread's CPU outside collections is about 4 s against HotSpot's 1.75 s, and
   boot plus exit 1.9 s against 1.1 s. Not a collector cost.

## The rows (2026-10-24, g1ep)

Host 64.177.43.150 (16 cores, shared, load average about 7). Branch `g1-evac-proxy-ki`, binary `ld20`
(the content of `c8a6828262`). HotSpot 25.0.4 with `-XX:+UseG1GC -Xmx1g`.

| | CratonVM G1 | HotSpot G1 |
|---|---|---|
| `R23W6KeepLong 20` (rounds of 2.4 M nodes in 20 s) | 12, 12 | 82, 82 |
| `G1epKeepRounds 10` wall (s) | 18.6, 19.0 | 2.6, 3.0 |
| `G1epKeepRounds 10` user CPU (s) | 50.1, 51.5 | 17.4, 17.8 |
| `G1epKeepRounds 10` pauses / `Pause Full` (`-Xlog:gc`) | 90-92 / 0 (9 starved fulls) | 77-79 / 9 |

`tools/probes/G1epKeepRounds.java` is the fixed-work twin of `R23W6KeepLong`: it runs N rounds
instead of N seconds, so a loaded host can compare CPU and wall per round.

**First young pause, 481 MB copied** (`scale.sh`, `CRATONVM_G1_WORKERS=w`, `R23W6KeepLong 1`):

| workers | 1 | 2 | 4 | 8 | 13 |
|---|---|---|---|---|---|
| `closure_us` | 1 469 884 | 725 660 | 386 015 | 298 047 | 268 789 |
| ns per byte | 3.05 | 1.51 | 0.80 | 0.62 | 0.56 |

The copy scales with workers: 5.5x at 13. HotSpot's first pause on the same row copied about
300 MB in 52 ms (0.17 ns/B) in the old page's Windows measurement, which was not re-taken on this
host. The gap at full width is therefore about 3.3x per byte. HotSpot's later young pauses here, read
from `-Xlog:gc` sizes and times, run at roughly 0.35-0.45 ns/B under this host's load.

Over a whole run (`G1epKeepRounds 10`, `phase.sh` sums of `[GC-STAT]`):
- Young pauses take 14.6-20 s of the run. Mixed and full pauses take 2-5 s.
- The young closure copies 8-9 GB at 1.3-1.9 ns/B. Most young pauses still run serial: in
  `young evacuation: parallel=N serial=M`, `serial` is about 85%.
- With `CRATONVM_G1_PARALLEL_UNDER_LOW_FREE_LATCH=1`, the closure halves (0.75-0.87 ns/B), but the
  parallel driver's unattributed time (`other_us`) grows from about 1.3 s to 4-6.7 s. Young time
  went from 22 s to 17.5-21.7 s, which is not a clear win.

## Where the copy's time goes (`perf record -e cpu-clock -g`, `G1epKeepRounds 4`, latch arm)

| symbol | share of all samples |
|---|---|
| `PointerMap` (`hashbrown` insert, `Equivalent::equivalent`, `rustc_entry`, `get`, rehash, `par_extend`) | about 19% |
| `SharedEvac::evacuate` (mostly the forwarding-claim CAS, the first touch of each object) | 8.3% |
| `classify_candidate_header_view` (the per-reference header screen) | 5.9% |
| `num_slots`, `ScanCache` TLS, `object_total_size`, `object_instance_size` (layout lookups) | about 10% |
| `concurrent_mark_step_worker`, `metadata_pin::roots_for_loader` | 8% |

JIT code is 2.3% of the main thread's samples. The gap is in the collector.

## Closes when

`G1epKeepRounds 10` takes at most 2x HotSpot's wall on the same host at the same time, with
`fact chains_ok=true`. A narrower milestone: the first-pause closure at 13 workers is at most
0.34 ns/B (2x HotSpot), and young pauses on a low Free pool run parallel without the `other_us`
growth.

**No longer blocks** the default-ON flip of `CRATONVM_G1_HEAP_RESIZE` (refused 2026-10-25 on
`BinTreesClassic 18` at 1.155x CPU; the g1sz soak read 0.983x and the flip landed):
`g1-default-sizing-never-collects-a-short-allocation-run`.

## What would have to exist

1. **A forwarding side table that is not a hash map of every copy.** The pause writes each forward
   into the object header (`publish_claimed_forwarding`) and also into the `PointerMap`. The map's
   inserts and probes are the largest single cost. The fix-up walk could read forwards from
   from-space headers, which stay intact until the free phase. The VM's root remap
   (`result.pointer_map`, with many consumers in `vm/`) is what requires the map. A per-pause
   address-keyed structure built lazily, or the VM consulting headers before the free phase, are
   the two shapes.
2. **The parallel driver's fixed cost on small pauses.** `other_us` is 24 ms per pause on the latch
   arm. The candidates are the shard merge into the `PointerMap` (`par_extend_pairs_with`) and the
   per-pause worker setup.
3. **The header screen.** `classify_candidate_header_view` runs once per CSet-bound reference.
   Before removing it, measure whether its cost is only the object's first cache miss moving
   elsewhere (its loads are the first touch of the header that `evacuate` reads next).
4. **Layout lookups.** `num_slots` / `ScanCache` resolve the class layout per object. A per-pause
   cache keyed by class id, or the size carried in the header for instances, would remove the TLS
   lookup.

Rejected or neutral this round (2026-10-24): `-XX:MaxTenuringThreshold=1` (copied bytes unchanged,
so survivor re-copying is not the volume), `CRATONVM_G1_EVAC_WORKERS_BY_POOL=0` together with the
latch (no gain beyond the latch alone), `CRATONVM_G1_IHOP_BACKOFF=1` (not relevant to this row).

## Row from the in-process `javac` retirement (2026-10-27)

`tools/probes/spring/ZipFsFinalizerChurn.java <300 jars> 60`, `-XX:+UseG1GC -Xmx512m`, host 64.177.43.150:
HotSpot 25 runs it in 15.6 s with 77 young pauses totalling 2.2 s; CratonVM (dev `6bb160f1b9`) takes 225 s with
280 young pauses totalling 61 s (median 210 ms), stdout identical. Each round drops 300 unclosed
`jdk.nio.zipfs.ZipFileSystem` objects, which have a `finalize()`: a young pause that finds them dead keeps their
entry indexes alive for finalization, and `G1Collector::resurrect_dead_finalizers` evacuates that graph on the
driver alone (`scan_and_evacuate_refs_tuned` -> `evacuate_object_tuned`), ~35 % of the allocation-path samples and
~12 % of the main thread. HotSpot keeps FinalReference referents alive in its parallel reference-processing phase.
The remark half of the same graph was fixed
.
