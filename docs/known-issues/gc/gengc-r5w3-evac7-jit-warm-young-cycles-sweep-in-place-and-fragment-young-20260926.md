# On a JIT-warm program no young cycle copies: the in-place sweep fragments young, and its serial whole-arena passes are the young pause

> **STATUS (2026-09-29, gce e2/y): OPEN (perf) -- the probe made decisive; e1 numbers pending medians.**
> - **e1 numbers** (orchestrator, JIT, 3 runs): 62/34/36 s against 93/40/39 on the base.
> - **Probe changes** (`GenR4W4EvacThroughputProbe`):
>   - `time_ms=` moved to STDERR, so stdout is exactly HotSpot's line and the rows can be SAME;
>   - one STDERR `[evac-timing] ... steady_median_ms=S` line per run (the main loop's tenths, JIT warm-up excluded);
>   - a SHORT arm `262144 4000000` (`checksum=6471610074841698382`, from the loop's exact model, which reproduces the three documented checksums) that stays under a minute with `--nojit`.
> - **Rows:** `evac_short_*` in section 3 of `docs/internal/gc-design-perf-round-20260929/e2-y-report.md`. Retire the page's e1 claims on the median `steady_median_ms` e1 < base, interleaved, JIT arm.
> - **Next cost:** the sweep's serial selective-promotion walk and fix-up 3a, unchanged. They are anchor-verified linear walks whose parallel form needs per-chunk sharding of the deferred installs and age bumps under the same unwind discipline; that is a design item, not landed blind.

> **STATUS (2026-09-29, gce e1/x): KEEP -- perf page; correctness holds, the medians are unread.** `divert_1..3` = HotSpot on both binaries; `evac_jit_1..3` rc 0 on both (`verify-e1/ve1`); `jitwarm_divert`, `jitwarm_divert_term4`, `w1_jitwarm_stress` = HotSpot on every e1 battery. **Remaining:** the `mark-closure` / `zero-and-publish` phase and `nonmoving-*` pause medians from `evac_jit_*` stderr against the base, and `[GC] evac_pool: pool_dispatches` rising per young cycle.

> **STATUS (2026-09-29, gce e1/y): OPEN (perf) -- two more default-path costs removed (behaviour-identical), item 3's zeroing half written (opt-in consumer pending), the rest narrowed.** Unbuilt when written.
>
> 1. **Fixed (default on): the sweep spawned OS threads on every cycle.** The mark closure (`young_mark::drain_parallel`, called up to five times per sweep: the closure, two late-resolution re-drains, the finalizer resurrection drain and the loader-rescue re-drain) and the reclaimed-span zeroing (`zero_spans_parallel`) each opened a `std::thread::scope` and spawned `young_gc_threads - 1` fresh threads per call. The chunked walks moved to the heap's persistent pool in gen r4/sweep (`run_chunk_workers`); these two were left behind. Now `drain_parallel_on` / `zero_spans_parallel_on` take the heap's `EvacPool` (`mark_pool` in `sweep_young_non_moving`, built only when `mark_threads > 1`). The worker count is `young_mark::phase_width`: the pool grows to the request (`ensure_helpers`) and a narrower pool (a refused OS thread) is what the drain's `live` count is sized from, so the termination protocol cannot wait for a worker that never starts. Same marked set, same zeroed bytes. On an 8-core host that is up to 42 thread creations per young cycle removed.
> 2. **Fixed (default on): the sweep's card re-dirty did a locked RMW per SLOT.** `redirty_cards` holds one entry per old->young slot the card scan found (~250k on `GenR4W4EvacThroughputProbe`), and `CardTable::mark_dirty_bulk` did a `compare_exchange` (a locked instruction even when it fails) plus two summary/bound probes for each. It now skips a repeat of the previous card and a card already dirty (no clear can run: every clear holds the `cells` lock the bulk call holds). Same dirty set, summary and scan bound.
> 3. **Written (item 3's zeroing half): the known-zero free list.** See the gce e1/y STATUS of `gcd-d4m-sweep-trigger-follows-the-moving-pause-goal-20260928.md`; the refill consumer is opt-in and lane o's.
> 4. **Still open:** the sweep's serial whole-arena passes (the selective-promotion walk, fix-up 3a) and the random card/3c passes. Parallelising the selective walk needs its deferred age bumps and pin decisions sharded per worker; that is the pin10 round-three proposal's item, unchanged.
>
> Tests: `cargo test -j 5 -p cratonvm-gc --lib young_mark::tests` (new: `e1y_pool_drain_visits_every_node_exactly_once_across_cycles`, `e1y_phase_width_is_what_the_pool_can_supply`, `e1y_a_panicking_scan_on_the_pool_propagates_and_the_pool_survives`, `e1y_zero_spans_on_the_pool_zero_exactly_the_spans`) and `cargo test -j 5 -p cratonvm-gc --lib gce_e1y_bulk_tests`. Verify: `CRATONVM_DBG=gcpause,gc-stats CRATONVM_DBG_GCPHASE=1 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4EvacThroughputProbe`, ABBA against the base binary: every run `PASS evac live=262144 iters=20000000 checksum=1021046735439613382 corrupt=0`; the `[gcphase] mark-closure:` and `zero-and-publish:` phases and the `nonmoving-*` `[gcpause]` medians no higher (expected lower on a many-core host); `[GC] evac_pool:` `pool_dispatches` now rises every young cycle (the mark and the zeroing dispatch on it). Also `BinT 14` and `GenR4W4JitWarmDivertProbe` (`PASS jitwarm threads=4 calls=1500000 checksum=-4668312146048759300`) under `CRATONVM_DBG=gc-stress=250000`.

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`): unchanged -- OPEN (perf); nothing flipped.** The sweep-arm trigger policy is `gcd-d4m-sweep-trigger-follows-the-moving-pause-goal-20260928.md` (opt-in, unmeasured on d7); `a_batched_freed_span_record_matches_the_per_span_one` passes in the round's Windows suite. The d7 battery's single `GenR4W4EvacThroughputProbe` runs (see the young4 parallel-evacuator page) decide nothing.

> **STATUS (2026-09-28, gcd d4/m): still OPEN (perf). One default-path cost
> removed (bit-identical). The largest term is the young TRIGGER on the sweep
> arm, a policy, filed. Nothing flipped.** Unbuilt when written.
>
> *Largest cost terms on the default path, by reading, largest first.* The
> numbers are the page's own estimates below, re-derived against the code at
> `d3e80a4ec`.
>
> 1. **Cycle count.** On the sweep arm the trigger is the pause-goal-adapted
>    MOVING threshold: `young_gc_trigger_bytes` takes `moving_threshold` when
>    `pause_goal_armed`, and `DEFAULT_YOUNG_PAUSE_GOAL_MS` = 200 arms it by
>    default. The sweep's pause is O(allocated), so the goal pulls the
>    trigger down. That gives about 130-170 cycles against HotSpot's ~34.
>    Every per-cycle cost below is paid 4-5x as often. A policy change, so
>    filed:
>    `gcd-d4m-sweep-trigger-follows-the-moving-pause-goal-20260928.md`.
> 2. **The sweep's serial whole-arena passes and random ring passes**
>    (selective promotion, fixup 3a, 3c, the card seed): 25-55 ms per cycle.
>    These are structural (`sweep_young_non_moving`). Parallelising them is
>    the pin10 round-three proposal's item.
> 3. **Mutator-side refills after a fragmenting sweep, and the second zeroing
>    of every reclaimed byte** (`zero_young_hand_out` on a free-list
>    hand-out, after the sweep's `zero_spans_parallel`). The zero-once skip
>    covers bump-tail hand-outs only. Extending it to swept blocks needs the
>    arena to carry a known-zero bit per free block, because
>    `Arena::add_free_block` has no such state. Filed on the same trigger
>    page as its item 2.
> 4. **Per-span bookkeeping in the sweep's publication loop.** One
>    process-wide `fetch_add` plus a pause-ledger coverage read per reclaimed
>    span, ~10^5 spans a cycle on this probe. **Fixed:** the loop now records
>    the whole `reclaimed_regions` list with one `record_young_spans_freed`
>    call (gcd d1/e's batched form), with the same ring contents. No switch:
>    the behaviour is identical.
>
> *Verify:*
>
> - `cargo test -j 5 -p cratonvm-gc --lib a_batched_freed_span_record_matches_the_per_span_one`
>   (the equivalence the batch rests on);
> - the probe, ABBA against `d3e80a4ec`: every run prints
>   `PASS evac live=262144 iters=20000000 checksum=1021046735439613382 corrupt=0`;
> - `time_ms` within noise or lower. The sweep's `[gcphase] zero-and-publish:`
>   lines (`CRATONVM_DBG_GCPHASE=1`) should drop by a few ms a cycle.
>
> *Previous status:*
>
> **STATUS (2026-09-27, gcd d2/h): still OPEN (perf), NARROWED further by
> reading. No code change on this page's default path this wave (the young
> copy's changes this wave are the cards3 producer and the sizer10 refusal,
> recorded on those pages); nothing flipped.**
>
> **Why a JIT-warm young cycle does not copy, by the code on `a1fa77603`
> (`collect_garbage_inner_with_pins`):** two terms of `divert_non_moving`,
> and only ONE of them has an escape hatch.
>
> 1. **Term 3, `divert_for_incomplete_moving_coverage`.** `coverage_incomplete`
>    ORs `gc_quiescence::takeover_forbids_unpinnable_move()`, which is
>    `TakeoverVerdict::licence(false) == NonMoving`: ANY frozen peer or helper
>    window (`frozen > 0 || helper_windows > 0`) forbids the move for a
>    backend that cannot pin, whatever the pins. That is the census reason the
>    orchestrator's base run printed most, `xt-helper-window-conservative-scan`
>    (`incomplete_reason::XT_HELPER_WINDOW`, recorded first-wins; the full set
>    is `moving_young_incomplete_reason_mask()`). The verdict is lane i's; what
>    the young collector does with it is: the sweep, always.
> 2. **Term 4, `unrewritable_conservative_jit_roots`.** Its hatches are the
>    pinned copy (`CRATONVM_GEN_PINNED_YOUNG_COPY`) and option B
>    (`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4`), both opt-in.
>
> **The pinned copy cannot take a term-3 cycle:** its candidacy is
> `term4_alone` (`unrewritable_conservative_jit_roots && !divert_for_incomplete_moving_coverage && ...`).
> So on this probe every helper-window cycle sweeps in place in BOTH arms,
> which bounds what the pinned arm can win (7.6-8.2 s -> 7.0-7.2 s measured):
> only the term-4-alone cycles convert. Whether helper windows are most of
> the non-moving cycles decides whether the flip plan
> (`../../internal/gc/gengc-r5w3-evac7-proposal-default-the-parallel-pinned-young-copy-DONE-20260928.md`)
> can close the gap at all. The counts that decide it, from ONE run of each
> arm (no timing needed):
>
> ```
> CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4EvacThroughputProbe
> CRATONVM_GEN_PINNED_YOUNG_COPY=1 CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL=1 CRATONVM_DBG=gc-stats \
>   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4EvacThroughputProbe
> ```
>
> Both print `PASS evac live=262144 iters=20000000 checksum=1021046735439613382 corrupt=0`.
> Read the decision histogram: `nonmoving-coverage-incomplete` (sub-reason
> `xt-helper-window-conservative-scan`) against
> `nonmoving-unrewritable-conservative-jit-roots` (default arm) and
> `moving-pinned-pages` (pinned arm). If the helper-window count dominates
> in the pinned arm, the gap is lane i's verdict plus the proposal filed this
> wave, `../../internal/gc/gcd-d2h-proposal-pinned-copy-takes-helper-window-cycles-DONE-20260928.md`;
> naming the peer is `CRATONVM_DBG_XT_JIT_ROOT_SCAN=1` (per-peer lines). A
> single-threaded probe has no application peer, so the window is a VM or JDK
> daemon thread blocked under a compiled frame; which one is the open
> question for lane i.
>
> **What remains in the copy's own cost** is unchanged from the d1/e block
> below (the per-survivor `pointer_map` insert, the opt-in scan prefetch, the
> double zeroing of reclaimed young bytes on the refill path).
>
> *Previous status, kept for the record:* **(2026-09-27, gcd d1/e) still
> OPEN (perf), NARROWED. Nothing here is a wrong answer; the default path is
> byte-identical.**
>
> **What stops the default path from copying, re-verified on `6d39e8dcc`:**
> term 4 alone (`collect_garbage_inner_with_pins`: `term4_population` =
> moving young, no `CRATONVM_GC_NO_PEER_PIN_DIVERT`, a compiled frame live,
> and a conservative JIT scan ran). Its two escape hatches are both opt-in:
> the pinned copy (`CRATONVM_GEN_PINNED_YOUNG_COPY`, flip plan in
> `../../internal/gc/gengc-r5w3-evac7-proposal-default-the-parallel-pinned-young-copy-DONE-20260928.md`)
> and option B (`CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4`). Flipping either is
> the orchestrator's triage, not a defect fix, so nothing was flipped.
>
> **Landed this lane (young copy only):**
>
> - Option B is now safe to measure: a cycle it lets through pins the
>   blocked peers' interior native-stack words (the pin8 page, fixed;
>   `plan_takes_blocked_peer_words`). Before, option B could relocate an
>   object under a blocked peer's interior cursor, which made it unusable as
>   an A/B arm for this page.
> - The pinned rebuild's per-span bookkeeping: `finish_in_place_young_cycle`
>   records its freed spans with the new `record_young_spans_freed` (one
>   `xt_cycle_coverage` read and ONE `fetch_add` per cycle, and no writes of
>   records the batch itself would overwrite) instead of
>   `record_young_span_freed` per gap. On this probe a pinned cycle frees one
>   span per gap between scattered survivors, i.e. up to hundreds of
>   thousands per cycle, each of which paid a process-wide locked RMW and a
>   thread-local ledger borrow plus six loads. Bit-identical ring contents.
>   Expected effect: small (well under the 3x noise in `time_ms`); read it in
>   the `cardclear+young_reset` median of the `moving-pinned-pages` cycles.
>
> **What remains in the copy's cost** (unchanged, owners named):
>
> - the per-survivor `pointer_map` insert (a design item, pinned5 residual 1:
>   the identity entries are load-bearing for reference processing,
>   finalizer resurrection and the loader rescue);
> - the scan prefetch is opt-in (`CRATONVM_GEN_EVAC_SCAN_PREFETCH`) and
>   unmeasured on the pinned arm;
> - the rebuild's zeroing of every dead byte is then repeated by
>   `zero_young_hand_out` on every free-list hand-out (item 3 above: the TLAB
>   refill, lane d's code);
> - items 2 and 4 above (the trigger and the sweep) are not the young copy's.
>
> Verify (unit): `cargo test -j 5 -p cratonvm-gc --lib a_batched_freed_span_record_matches_the_per_span_one`
> and `cargo test -j 5 -p cratonvm-gc --lib r4w5_pinned_young_copy` pass.
> Runtime: the command in the pin10 block below, ABBA against `6d39e8dcc`,
> must print
> `PASS evac live=262144 iters=20000000 checksum=1021046735439613382 corrupt=0`
> on every run in both arms.

> **ORCHESTRATOR RUNS (2026-09-27, Linux release, same host, interleaved;
> host noise is about 3x):**
>
> | Build | Default | `CRATONVM_GEN_PINNED_YOUNG_COPY=1` |
> |---|---|---|
> | Wave-4 | 14.5 s | 11.2 s (with the parallel arm) |
> | Wave-5 `g6w5c` (gate G6, wall) | 10-12 s | 10-11 s |
> | Round-5 tip `c6c98c760` (`time_ms`) | 7.6-8.2 s | 7.0-7.2 s |
>
> The probe is `GenR4W4EvacThroughputProbe`. HotSpot Serial takes 3 s. The
> checksum is identical on every arm and every run reports `corrupt=0`.
>
> The gap to HotSpot is down from about 5x to about 2.5x. The page stays open
> until the pinned copy (or a moving default) closes the rest.

> **Earlier status (2026-09-27, gen r5w6/pin10; superseded by the block at the top): still OPEN. The pinned copy's own
> per-cycle costs are lower; the sweep is untouched.**
>
> The sweep (`sweep_young_non_moving`), the trigger and the refill path are
> other lanes' code. This lane owns the young copy. What landed there is
> bit-identical, and on by default:
>
> - **The copy.** Every survivor's destination mark takes ONE store: the
>   age or old-gen flag is folded into the snapshot first. This replaces a
>   store plus a locked RMW. It applies to both the serial and the parallel
>   evacuator, and to the Cheney and pinned arms.
> - **The pinned rebuild.** `finish_in_place_young_cycle` sorts `live` with the
>   run-merging stable sort. The list is a handful of ascending runs (see its
>   comment), so the sort is O(n log runs) instead of O(n log n). Its time is
>   in the `cardclear+young_reset` row.
> - **Re-encounters.** On a pinned cycle, a re-encounter of a copy made into a
>   from-space span no longer probes `pointer_map`.
>
> Expected effect on `GenR4W4EvacThroughputProbe` with the pinned arm: a few
> percent of the 11.2 s. That is roughly 37 M survivors x one locked RMW, plus
> ~150 sorts of ~200 k entries. It is not the 3.7x gap. The gap's named causes
> are unchanged: the divert, the trigger at half the semi-space, and survivors
> copied three times. They are proposals in the other lanes' files, collected
> in `gengc-r5w6-pin10-proposal-young-copy-round-three-20260927.md`.
>
> A/B (ABBA, base `fbcb272ab` against this lane's commit, one host). Every
> run must print
> `PASS evac live=262144 iters=20000000 checksum=1021046735439613382 corrupt=0`:
>
> ```
> CRATONVM_GEN_PINNED_YOUNG_COPY=1 CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL=1 CRATONVM_GC_PAR_EVAC_CARD_SEED=1 \
>   CRATONVM_DBG=gcpause,gc-stats CRATONVM_DBG_GCPAUSE_MIN_US=0 cratonvm --java-home "$JDK" \
>   -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4EvacThroughputProbe
> ```
>
> Compare `time_ms`, and the `evac_drain` and `cardclear+young_reset` medians
> of the `moving-pinned-pages` cycles. The default arm (no flags) should read
> the same as the base, within noise: once warm it runs the sweep, which
> nothing here touched.

*Filed 2026-09-26 by gen round 5, wave 3, lane `evac7`.*

- **Status:** open (perf). Nothing here is a wrong answer.
- **Severity:** perf. The lane's read of where `GenR4W4EvacThroughputProbe`'s
  ~13 s gap to HotSpot Serial (16 s vs 3 s, same checksum) goes.
- **Backend:** Generational, default flags.
- **Code:**
  - `gc/src/gen_heap.rs` `collect_garbage_inner_with_pins`: the divert
    (term 4, `unrewritable_conservative_jit_roots`).
  - `gc/src/gen_heap.rs` `sweep_young_non_moving`: the cycle that runs
    instead.
  - `gc/src/gen_heap.rs` `needs_gc`: the young trigger.
  - `gc/src/gen_heap.rs` `refill_tlab` / `refill_fragmentation_fallback`:
    allocation after the sweep.

Everything below was read from the code at `ebdc885de`. Nothing was run. The
counts are estimates from the probe's shape, and the command at the end
confirms or refutes each one.

## What is wrong

**1. The probe never copies.**

- Its main loop is compiled (OSR). A young collection triggered from that loop
  has `has_conservative_roots` true and `conservative_jit_scans() > 0`.
- Term 4 therefore fires on every cycle. `CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4`
  and `CRATONVM_GEN_PINNED_YOUNG_COPY` are both off by default, so the cycle
  runs `sweep_young_non_moving`.
- The same outcome was measured on `GenR4W4JitWarmDivertProbe` at `ebdc885de`:
  `moving=0` of 118.
- Consequence: every young-copy speed-up (the parallel evacuator, the serial
  promotion buffer, this wave's flags) is inert on the default path of any
  JIT-warm program.

**2. In-place retention fragments young, and the trigger then fires early.**

- Each iteration allocates 120 B: a `Node` (24 B compact), an `int[4]`
  (32 B) and a dead `byte[48]` (64 B).
- The live ring nodes replaced in the last three cycles stay young
  (`PROMOTION_AGE` 3). They sit where they were allocated, one pair every
  ~120-360 B, between dead `byte[48]` holes.
- The sweep reclaims the holes into the free list. Its selective promotion
  moves only survivors that are aged and unpinned. The defragmentation
  escalation (every unpinned survivor promoted) fires only when no free block
  of at least 64 KiB exists AND the bump cursor is exhausted.
- The young trigger counts live bytes (`used - free_list_bytes`). The default
  pause goal (`DEFAULT_YOUNG_PAUSE_GOAL_MS` = 200) caps it at the adaptive
  moving threshold (at most 50 % = 32 MiB of a 64 MiB semi-space) even on
  the non-moving arm (`young_gc_trigger_bytes`).
- With ~14-18 MB of survivors retained in place, a cycle therefore follows
  every ~14-18 MiB of allocation. That is about 130-170 young cycles for the
  run's 2.4 GB, against about 34 for HotSpot's 68 MB eden.

**3. After such a sweep, allocation is slow.**

- TLABs are carved from the free list first. A refill that finds no large
  block serves the largest one (`refill_fragmentation_fallback`, floor 256 B).
- Holes under 256 B are reachable only by the per-object slow path under the
  young lock.
- Every free-list hand-out is zeroed again by `zero_young_hand_out`. The
  sweep has already zeroed those bytes, so each reclaimed byte is zeroed
  twice, and the second time serially on the mutator.
- On the non-escalated cycles this is tens of thousands of mini-TLAB refills
  per epoch.

**4. The sweep's per-cycle cost is O(allocated), and mostly serial.** By
reading, in phase order:

- **A serial dirty-card seed.** About 250k `mark_edge_precise` calls, each a
  random read of a ring target's header.
- **A serial selective-promotion walk of all of `[0, used)`.** About 1.2-1.4M
  headers.
- **A second serial walk when anything was promoted.** Fixup 3a.
- **A serial pass over every ring slot (3c).** Each slot's `fwd_of` is a
  random header read.
- **The zeroing of every dead byte.** About 40-55 MB (parallel).
- **The free list, collected and sorted twice.** It is published twice (one
  `add_free_block` per region).

## Estimated breakdown (by reading; per young cycle unless stated)

| cost | where | estimate |
|---|---|---|
| mutator-side: mini-TLAB refills, slow-path allocations, second zeroing | after each non-escalated sweep | 20-60 ms of mutator time per epoch |
| two serial whole-from-space walks | selective promotion (2), fixup (3a) | 15-30 ms |
| two serial random passes over ~250k ring targets | card seed, 3c | 10-25 ms |
| zeroing 40-55 MB | `zero_spans_parallel` | 5-10 ms |
| free-list rebuild ×2, anchor sort | reclaim, arena coalesce | 3-8 ms |
| cycles per run | the trigger (item 2) | ~130-170 (HotSpot ~34) |

Summed, that is ~60-120 ms per cycle over ~150 cycles, or 9-18 s. This
brackets the measured 13 s gap. The mutator's own compiled-code speed was not
examined.

## Top three costs, named

1. **The divert itself.** In-place retention fragments young and halves the
   allocation between cycles. The mutator then pays the fragmentation in
   refills and slow paths.
2. **The sweep's serial O(allocated) passes.** Two whole-arena walks and two
   random ring passes per cycle.
3. **When a cycle does copy** (warm-up, `--nojit`, or the pinned copy below),
   the dirty-card roots are forwarded on ONE thread before the parallel drain
   opens: half the survivors on this probe. On top of that, the evacuated
   semi-space is decommitted and re-faulted every cycle.
   See `gengc-r4w4-young4-parallel-evacuator-scaling-limits-20260924.md`'s
   STATUS for the moving cycle's own breakdown.

## Proposed fix

- **Make term-4 cycles copy.** This is the pinned in-place young copy
  (`CRATONVM_GEN_PINNED_YOUNG_COPY`), now with its parallel arm
  (`CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL`, gen r5w3/evac7) and the parallel
  card seed (`CRATONVM_GC_PAR_EVAC_CARD_SEED`).
  - A pinned cycle compacts survivors into the free spans. Young then holds a
    few large free blocks, not tens of thousands of holes.
  - The mutator allocates from full-size TLABs again.
  - None of the sweep's serial passes run.
  - The flip plan and its gates are in
    `../../internal/gc/gengc-r5w3-evac7-proposal-default-the-parallel-pinned-young-copy-DONE-20260928.md`.
- **Independently, for the sweep itself** (not this lane's code):
  - parallelise the selective-promotion walk and fixup 3a on the anchor grid
    the parallel sweep already uses;
  - skip `zero_young_hand_out` for free-list blocks the sweep zeroed this
    epoch (a per-block "zeroed at epoch E" tag);
  - let the defragmentation escalation also trigger on a hole-count or
    mean-hole-size bound, not only on "no 64 KiB block".

## How to verify

```
CRATONVM_DBG=gcpause,gc-stats CRATONVM_DBG_GCPHASE=1 cratonvm --java-home "$JDK" \
  -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4EvacThroughputProbe
```

- Must print
  `PASS evac live=262144 iters=20000000 checksum=1021046735439613382 corrupt=0`.
- The `[GC]` summary should show the young cycles as
  `nonmoving-unrewritable-conservative-jit-roots` (or `coverage-incomplete`).
  That confirms item 1.
- `selective_promotion_census` `sweeps_defrag` against `sweeps` gives the
  escalation rhythm.
- The `[gcphase]` rows give the per-phase split.
- The minor count should be ~130-170 (item 2).

Then run the same command with
`CRATONVM_GEN_PINNED_YOUNG_COPY=1 CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL=1 CRATONVM_GC_PAR_EVAC_CARD_SEED=1`.
The same PASS line must print, the cycles must read `moving-pinned-pages`, and
`time_ms` is the number to compare (ABBA, interleaved; in-JVM timings swing
~3x between repetitions on the shared box).
