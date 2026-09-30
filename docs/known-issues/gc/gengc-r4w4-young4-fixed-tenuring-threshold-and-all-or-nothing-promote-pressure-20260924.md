# The tenuring threshold is a constant, and the only adaptivity is an all-or-nothing arm

> **STATUS (2026-09-29, gce ve2): OPEN -- keep CRATONVM_GC_ADAPTIVE_TENURING off: promoted bytes do not drop (T1 252232472 vs T0 252232424); items 6 and 7 pass.** Evidence: `docs/internal/gc-design-perf-round-20260929/ve2-verdicts.md`; next steps: `gce-handoff-gc-design-perf-round-close-20260929.md`.

> **STATUS (2026-09-29, gce e2/y): OPEN (the flip) -- items 4-8 as battery rows, with the counters item 4 lacked.** No summary line printed promotion totals, so "objects_promoted lower on the flag arm" could not be read from a log. The `[GC] gen-alloc:` line (`CRATONVM_DBG=gc-stats`) now ends in `gen_objects_promoted= gen_bytes_promoted= gen_tenuring_threshold= gen_tenuring_adapted= gen_tenuring_raised= gen_tenuring_lowered=` (`alloc_path_census_line`, test `gce_e2y_tests::e2y_the_gen_alloc_line_carries_the_tenuring_gate_counters`). Section 2 of `docs/internal/gc-design-perf-round-20260929/e2-y-report.md` has every row (T0/T1 interleaved) and its bound:
> - item 4: `gen_bytes_promoted` and `oldsz_committed_peak` on SurvivorOverflow; evac `steady_median_ms` within 10 %, JIT and `--nojit`;
> - item 5: BinT under stress;
> - item 6: the OOM ladder;
> - item 7: `major=` on MajorCadence, plus OldGenFinalize;
> - the SoftRefLru and TenuringProbe controls.
>
> Item 8 (netty, Spring Boot) stays a host item.

> **STATUS (2026-09-29, gce e1/x): KEEP -- no gate item was run.** The battery's `tenuring_default` / `tenuring_adaptive` rows end rc 0 with the same verdict on base and e1. **Remaining:** flip gate items 4-8 of `CRATONVM_GC_ADAPTIVE_TENURING`.

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`, wave d7, Linux release): unchanged -- FIX LANDED OPT-IN (`CRATONVM_GC_ADAPTIVE_TENURING`); OPEN only as the flip.** The blocker stays closed: `GenR4SoftRefLruProbe` with the adaptive arm is =HS 3/3 (`d4n_softlru_adaptive_1..3`). The battery shows the difference the flip would make on `GenR4W6TenuringProbe -Xmx256m`: default `medium-lived=promoted old-growth-pct=96` (`tenuring_default`), adaptive `medium-lived=died-young old-growth-pct=0` (`tenuring_adaptive`), identical checksums. Remaining: the flip gate below (triage FLIP_PENDING).

## STATUS (2026-09-28, gcd d5/r): FIX LANDED (opt-in `CRATONVM_GC_ADAPTIVE_TENURING`); its one measured blocker is FIXED (d4/n root cause, applied in d916d1c40, confirmed 3/3); re-read, nothing else blocks; the FLIP GATE is below

- **The blocker, closed.** The triage held the flip because
  `GenR4SoftRefLruProbe` failed on the adaptive arm. Root cause (d4/n):
  `VmHeap::soft_ref_policy_free_mb` read young's bump high-water, so after a
  non-moving young sweep young looked full and HotSpot's LRU condemned every
  idle soft reference; adaptive tenuring kept the probe's referent young
  through more such sweeps. Fixed in `gc/src/vm_heap.rs` (subtracts
  `young_from_free_bytes()`), default on. Orchestrator, d916d1c40:
  `CRATONVM_GC_ADAPTIVE_TENURING=1 ... -Xmx256m GenR4SoftRefLruProbe` prints
  HotSpot's lines 3/3; `GenR4W6TenuringProbe` gives HotSpot's verdict token
  (`medium-lived=died-young`), its `old-growth-pct` number differing as
  before (not a comparable value).
- **Re-read of `gc/src/gen_heap_tenuring.rs` on d916d1c40 for anything else
  that blocks:** none found. Notes for the flip, none a blocker:
  1. the threshold starts at `MaxTenuringThreshold` (15, i.e.
     `promotion_age` 16, representable: `MAX_GC_AGE` 15 + 1), as Serial/G1;
  2. the non-moving sweep feeds the age table (`1f3cf16f4`) and applies the
     last computed threshold; the severe arm stays the one-cycle `T = 0`
     floor through `force_promote_all`, so `BinT 14`'s death-spiral break
     is kept;
  3. the parallel age census counts a forwarding-CAS loser twice (bias
     down, bounded by `EVAC_CAS_LOSSES`; the young5 block below);
  4. survivors stay young up to 15 cycles instead of 3: on JIT-warm
     (non-moving) cycles that is more in-place young occupancy and free-list
     fragmentation, which is what gate item 4 measures; and d4/n's true-root
     seed benefits (fewer promoting ladder majors fall back to the legacy
     seed);
  5. the state is per heap (`TenuringState`), no process global.
- **FLIP GATE** (Linux release, one binary, `-XX:+UseGenerationalGC`, arms
  `""` and `CRATONVM_GC_ADAPTIVE_TENURING=1` interleaved, JIT on unless
  stated; stdout must equal the no-flag arm and HotSpot Serial where the
  probe defines it):
  1. `cargo test -j 5 -p cratonvm-gc --lib gen_heap_tenuring` and
     `cargo test -j 5 -p cratonvm-gc --lib gen_r4w6_young6` pass.
  2. `GenR4W6TenuringProbe -Xmx256m`, 3/3: `CHECKSUM tenuring warmup=1024
     iters=8192 life=128 checksum=1741697103872 corrupt=0` and
     `VERDICT tenuring medium-lived=died-young` (the flag arm; the default
     arm's `promoted` is the control).
  3. `GenR4SoftRefLruProbe -Xmx256m`, 3/3, both arms: HotSpot's lines.
  4. `GenR4W4SurvivorOverflowProbe` (checksum `1181091290535902592`;
     `objects_promoted` and the old-gen peak lower on the flag arm) and
     `GenR4W4EvacThroughputProbe` (`PASS evac live=262144 iters=20000000
     checksum=1021046735439613382 corrupt=0`; median wall time of 5 within
     10 % of the default arm), JIT on and `--nojit`.
  5. `BinT 14` under `CRATONVM_DBG_GC_STRESS=250000`: `327670`, 3/3.
  6. OOM ladder, flag arm, same pass rate as the default arm (5 runs):
     `GenR4W4NativeStringOomProbe -Xmx64m` (HotSpot's four lines),
     `GenR4W4HeapFullThrashProbe -Xmx64m`, `GenR4W5ThreadsOomProbe -Xmx128m`.
  7. `GenR4W5MajorCadenceProbe`: majors not above the default arm;
     `OldGenFinalizeProbe` unchanged (0/64 on its allocation-driven arm both
     ways; that is the young-sizing residual, not this flag).
  8. Real workloads (Linux host): netty `io.netty.buffer` x4 and the Spring
     Boot sample, same pass counts, no fault.
- **The flip:** `gc_adaptive_tenuring` in `types/src/flags.rs` from
  `non_empty_non_zero` to `on_unless_zero` (`=0` restores the fixed age 3;
  no `types` test pins its default today; its `flag_groups.rs` entry
  already has `off_word: Some("0")`), `docs/config` regenerated, and the
  `gen_r4w6_young6` tests that pin the DEFAULT
  (`the_default_heap_keeps_the_fixed_promotion_age`,
  `the_distribution_print_alone_does_not_change_tenuring`) rewritten: they
  read `gc_flags()`, which is parsed once per process, so a thread override
  cannot restore the old default for them; assert the fixed arm through
  `TenuringState::promotion_age(false, PROMOTION_AGE)` or run them in a
  process with `CRATONVM_GC_ADAPTIVE_TENURING=0`. Owner: orchestrator.

## STATUS (2026-09-26, gen r5w1/young5, superseded above): FIX LANDED (opt-in), awaiting the default-flip A/B; re-read, probes correct

Re-read against `9e252c8b2`: adaptive tenuring, the age table on both moving
evacuators and on the non-moving sweep (the orchestrator's `1f3cf16f4`), and
the probe verdicts below are consistent with the code. One accuracy note for
the A/B, unchanged by this lane: on a PARALLEL cycle the driver's age census
counts a forwarding-CAS loser's object twice (the loser records the winner's
pair too), which biases the threshold slightly down; `EVAC_CAS_LOSSES` bounds
it (see `../../internal/gc/gengc-r5w1-young5-proposal-evacuator-contained-wins-DONE-20260928.md`,
"Also noted"). Also: `docs/config/flag-inventory.md` lists
`CRATONVM_GC_ADAPTIVE_TENURING` as `default-on`, which it is not (filed:
`../../internal/gc/gengc-r5w1-young5-flag-inventory-default-column-misreads-off-word-FIXED-20260927.md`).

---

*Filed 2026-09-24 by generational GC round 4, wave 4, lane `young4`.*
*Earlier status (superseded by the block above): **FIX LANDED (opt-in), awaiting probe:**
`CRATONVM_GC_ADAPTIVE_TENURING=1 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W6TenuringProbe`
must print `CHECKSUM tenuring warmup=1024 iters=8192 life=128 checksum=1741697103872 corrupt=0`
and `VERDICT tenuring medium-lived=died-young ...`, where the same command
without the variable prints `medium-lived=promoted` (gen r4w6/young6; see the
2026-09-24 wave-6 section at the end).* Severity: **perf / footprint**
(premature promotion into an old generation that only a major reclaims; or,
the other way, survivors copied semispace to semispace for no gain).*

## Code location

* `gc/src/gen_heap.rs`: `PROMOTION_AGE = 3` (a `const`), `should_tenure`,
  and the promote-on-pressure producer at the end of the moving arm
  (`freed_percent < GC_PROMOTE_PRESSURE_PERCENT && bytes_before >=
  from_cap_before / 2` arms `force_promote_all`).
* `gc/src/gen_evac.rs`: `ParEvac::promotion_age`, fed the same constant.
* The non-moving sweep's selective promotion reads the same constant through
  `should_tenure`.

## What HotSpot does, and what this collector does instead

HotSpot keeps an **age table** per young collection (bytes surviving at each
age) and recomputes the tenuring threshold after every collection: the
smallest age at which the cumulative surviving bytes exceed
`TargetSurvivorRatio` (50 %) of the survivor space, capped by
`MaxTenuringThreshold` (15). A survivor space that overflows anyway tenures the
overflow early ("premature promotion"), and `-Xlog:gc+age` prints the table.

CratonVM's young generation has no survivor space to overflow — to-space is as
large as from-space, so the Cheney copy always fits (the "Cheney invariant"
backstop refuses the cycle rather than overflow). Its two tenuring inputs are:

1. **A fixed age, 3.** An object is copied twice and promoted on its third
   survival, whatever the survivor volume. `MAX_GC_AGE` is 15 (4 bits), so a
   dynamic threshold up to 15 is representable today.
2. **The promote-on-pressure arm**: one cycle with >75 % survival at >= half
   occupancy makes the NEXT cycle tenure every survivor — i.e. the threshold
   drops to 0 for exactly one cycle, then snaps back to 3. (Round 4 wave 4 made
   the non-moving sweep honour it too and deleted the stale carry; see
   `docs/internal/gc/gengc-r4-move-stale-promote-on-pressure-arm-FIXED-20260924.md`.)

Consequences, by reading:

* **Medium-lived objects are promoted prematurely** whenever the arm fires:
  the cycle after a survival spike tenures objects of age 0 that would have
  died in the nursery one cycle later (the `MEDIUM-LIVED` phase of
  `tools/bench/GenR4W4SurvivorOverflowProbe.java` is built to show it).
* **A steady large live set is copied twice before promotion** on every
  workload where the arm does not fire (survival below 75 %): with a big
  nursery that is `2 x live` bytes of copying per object lifetime that
  HotSpot's threshold, computed against a small target, would have cut to one.
* The pause-goal loop (`adapt_young_trigger_to_pause`) and the arm act on the
  same symptom (survivor volume) through different levers and know nothing of
  each other.

## Proposal

1. **An age table, per cycle.** Sixteen `u64` byte counters beside
   `COPY_TALLY` (serial) and `EvacShard::tally` (parallel; merged by
   `copy_tally_merge`), bumped by the survivor's age at copy time. One add per
   copied object on a path that already does two; print it under
   `CRATONVM_DBG=gcpause` as `ages=[...]`.
2. **A per-heap dynamic threshold** (`AtomicU8`, initial 3): after each moving
   cycle, `threshold = min(age : sum(bytes[0..=age]) > target)`, `target =
   CRATONVM_GC_TARGET_SURVIVOR_PERCENT` (new flag, default 50) of the young
   semispace capacity **divided by the pause-goal loop's survivor latch** when
   that loop is armed, capped at `MAX_GC_AGE`. `should_tenure` gets it as its
   `promotion_age` argument on all three paths (it already takes one).
3. **Retire the arm into the threshold**: the >75 % case is exactly
   `threshold := 0` for one cycle; expressing it through the same variable
   removes the second mechanism and its census.
4. Keep `PROMOTION_AGE` as the floor/initial value and a
   `CRATONVM_GC_TENURING_THRESHOLD=<n>` fixed-threshold A/B lever.

## How to verify

`tools/bench/GenR4W4SurvivorOverflowProbe.java` (checksum
`1181091290535902592`) and `GenR4W4EvacThroughputProbe` (checksum
`1021046735439613382`) under `CRATONVM_DBG=gcpause,gc-stats`, against HotSpot
`-Xlog:gc+age=trace`: `objects_promoted`, the `Tenured Gen` peak and the
major count must fall on the overflow probe without `cheney_drain` rising on
the throughput probe; `BinT 14` (`327670`) must keep its death-spiral break
(its tree survives at >75 %, so the threshold must reach 0 there).

---

## 2026-09-24 round 4 wave 6 (lane `young6`): adaptive tenuring landed, opt-in

Verified first against `28f4acd3a`: `PROMOTION_AGE = 3` was still the only
tenuring input besides the one-shot `force_promote_all` arm, on all three
paths (serial `forward_object_impl`, parallel `ParEvac::evacuate` via its
`promotion_age` argument, and the non-moving selective promotion). The page
was accurate.

### What landed

* **`gc/src/gen_heap_tenuring.rs`** (new, included from `gen_heap.rs` as
  `mod tenuring`): HotSpot's `AgeTable::compute_tenuring_threshold` in
  HotSpot units (`compute_tenuring_threshold`), the desired survivor size, the
  per-heap `TenuringState` (never a process global), the
  `TenuringConfig` the launcher fills, and the `-XX:+PrintTenuringDistribution`
  lines. Unit conversion: this collector's `promotion_age` is HotSpot's
  threshold `T` + 1 (`should_tenure` asks `gc_age + 1 >= promotion_age`,
  HotSpot asks `age >= T`), so the historical 3 is `T = 2` and HotSpot's
  default ceiling 15 is 16.
* **The survivor target.** To-space is as large as from-space, so the
  survivor space's capacity is the wrong denominator. The target is sized
  against EDEN's equivalent, the young trigger: `desired = trigger / 8 *
  TargetSurvivorRatio / 100` (HotSpot's `SurvivorRatio = 8`), 2 MiB at the
  default 64 MiB semispace. When the pause-goal loop halves the trigger, the
  target halves with it (the page's proposal 2, without a second latch).
* **The age table**: `PromotionQueue::ages` (serial copy, counted at the new
  age in `forward_object_impl`) and, on a parallel cycle, filled by the
  driver's shard merge from each to-space forward's header. Filled only when
  something reads it (`PromotionQueue::age_census`).
* **One promotion age per cycle** (`GenerationalHeap::effective_promotion_age`),
  passed to both moving evacuators (`CopyPolicy::promotion_age`,
  `ParEvac::new`) and read by the non-moving sweep (`promotion_age_reachable`
  and its `should_tenure` call). The non-moving sweep does not recompute the
  threshold (no age table); it applies the last moving cycle's.
* **The graded response.** The age table is the grading: the more bytes
  survive at low ages the lower the threshold, one age at a time, recovering
  the same way. The severe arm (>75 % survival at >= half occupancy) is KEPT
  as the one-cycle `T = 0` floor through the existing `force_promote_all`
  (proposal 3's "`threshold := 0` for one cycle", and `BinT 14`'s death-spiral
  break), so its census and its non-moving consumption are unchanged. The
  moderate band (50-75 % survival) needs no arm of its own: with >= 25 % of the
  semispace surviving (>= 8x the target), the cumulative sum crosses the
  target at the first well-populated age, so the table already drops the
  threshold there (`T = 1` when the survivors are new).
* **Opt-in**, because nothing was measured: `CRATONVM_GC_ADAPTIVE_TENURING`
  (declared in `flags.rs`, `flag_groups.rs` token `adaptive-tenuring`,
  `flag-surface.txt`), or per VM any of `-XX:MaxTenuringThreshold`,
  `-XX:InitialTenuringThreshold`, `-XX:TargetSurvivorRatio` (an operator who
  typed a HotSpot tenuring flag asked for HotSpot's adaptive semantics).
  `-XX:+PrintTenuringDistribution` prints the table after every moving young
  cycle (`[GC] tenuring: Desired survivor size ... new threshold T (max
  threshold M)` and `- age a: bytes, total` lines) without engaging the
  policy; `--verbose:gc` prints it when the policy is engaged.
* **Plumbing**: `vm-cli` keeps the four flags verbatim through the normaliser
  (`is_tenuring_flag`) and extracts them in `extract_hotspot_flags`;
  `tenuring_config_from_flags` validates HotSpot's ranges (warn and ignore
  out-of-range, as for `-XX:NewRatio`); `VmConfig::tenuring` carries them;
  `vm_init` applies them with `VmHeap::set_tenuring_config` right after the
  heap is built. Non-Generational collectors print the one-line "honoured by
  the Generational collector only" note.
* **Also**: `VmHeap::tenuring_threshold` (the JFR `tenuringThreshold` source)
  now reports the heap's live value instead of the constant.

### Not done (proposals kept)

* Proposal 4's `CRATONVM_GC_TENURING_THRESHOLD=<n>` fixed lever: covered by
  `-XX:InitialTenuringThreshold=n -XX:MaxTenuringThreshold=n` (an adaptive
  threshold capped at `n`), so not added.
* The parallel age table is computed on the DRIVER (one header read per
  to-space survivor, only when engaged). Moving it into `EvacShard` is a
  cross-lane request to `gen_evac.rs`'s owner (young6 review, request X1).

### Tests

`gen_heap_tenuring::tests::*` (the rule, the target, engagement and units,
`adapt`, the print format, the flag ranges);
`gen_heap::tests::gen_r4w6_young6::{the_default_heap_keeps_the_fixed_promotion_age,
a_small_survivor_set_stays_young_past_the_fixed_age,
a_large_survivor_set_lowers_the_threshold_through_the_serial_evacuator,
a_large_survivor_set_lowers_the_threshold_through_the_parallel_evacuator,
max_tenuring_threshold_zero_tenures_on_the_first_survival,
the_distribution_print_alone_does_not_change_tenuring}`;
`vm-cli` `young6_tenuring_flags_survive_full_launcher_pipeline`.

### How to decide the default flip

ABBA on one binary, `-XX:+UseGenerationalGC -Xmx256m`,
`CRATONVM_DBG=gc-stats,gcpause`, with and without
`CRATONVM_GC_ADAPTIVE_TENURING=1`: `GenR4W6TenuringProbe` (verdict above),
`GenR4W4SurvivorOverflowProbe` (checksum `1181091290535902592`; `objects_promoted`
and the `Tenured Gen` peak must fall), `GenR4W4EvacThroughputProbe` (checksum
`1021046735439613382`; `cheney_drain` must not rise by more than noise),
`BinT 14` (`327670`; the severe arm still fires), and the old-gen cadence
probe `GenR4W5MajorCadenceProbe` (fewer majors expected).

## Orchestrator measurement (wave 6 final, binary `w6m`)

`GenR4W6TenuringProbe`, `-XX:+UseGenerationalGC -Xmx256m`. The checksum
`1741697103872` matches HotSpot on every arm.

| Arm | Verdict |
|---|---|
| default | `medium-lived=promoted` |
| `CRATONVM_GC_ADAPTIVE_TENURING=1` | `medium-lived=died-young` |
| `-XX:MaxTenuringThreshold=15` | `medium-lived=died-young` |
| `CRATONVM_GC_ADAPTIVE_TENURING=0` | `medium-lived=promoted` (the `=0` fix) |
| HotSpot G1 | `medium-lived=died-young` |

`BinT 14` under `gc-stress=250000` with the flag: `sum=327670`.
`MtChurnProbe` with the flag: `MTCHURN_OK`. `GenR4W4EvacThroughputProbe` (pool
wake-up change): checksum `1021046735439613382` at the default and at
`CRATONVM_GC_PAR_THREADS=16`.

**Found by the probe run: a JIT-warm probe diverts every cycle to the
non-moving path, and that path fills no age table.** Without
`CRATONVM_GEN_PINNED_YOUNG_COPY=1`, every young cycle of this probe is
`kind=non-moving divert=nonmoving-unrewritable-conservative-jit-roots`. So
`-XX:+PrintTenuringDistribution` prints nothing, and the threshold never
adapts: it stays at its initial value, `MaxTenuringThreshold`, which is why the
adaptive arm reads `died-young` there. With the pinned copy the cycles move,
and the distribution prints as HotSpot's does. For example, `Desired survivor
size 2097152 bytes, new threshold 2 (max threshold 2)` is followed by one
`- age N:` line per age. Residual: the non-moving sweep should feed the age
table too (`sweep_young_non_moving` already applies the promotion age).

## After wave 6 (orchestrator, 2026-09-25): the non-moving sweep feeds the age table

The residual above is fixed in `1f3cf16f4`. `sweep_young_non_moving`'s
`age_bumps`, the only place that path ages a survivor, now also fill the age
table at each survivor's new age. The threshold is recomputed, or the fixed
decision printed, exactly as on the moving path.

Measured on `w7c`, without the pinned copy, where every cycle of
`GenR4W6TenuringProbe` is non-moving:
- `-XX:+PrintTenuringDistribution --verbose:gc` prints 274 `Desired survivor
  size` blocks;
- the default gives `medium-lived=promoted`, and `CRATONVM_GC_ADAPTIVE_TENURING=1`
  gives `medium-lived=died-young`, with the HotSpot checksum on both.
