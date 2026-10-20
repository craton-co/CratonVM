# Young collection residuals after gcj wave j3 (yg3)

> **STATUS (2026-10-20, gcj j3/yg3, rows on j3a): OPEN.** R1 narrowed: the two mirrors deferred per scan are non-strong hidden-class mirrors, not user-loader ones (`yg3_md_kill_dbg`: no `[DBG_MIRRORPIN] roots: user class=` line, while `young_mirrors_deferred` grows by 2 per scan). Why they stay young is still open: they are covered by `loader_pin` rows, not `mirror_pin`. R2 changed: the default is the forced sweep again (the rooting fix was refuted, dead loaders kept); its opt-in replacement `CRATONVM_GEN_YOUNG_MIRROR_DEFER_MOVING_FOLLOWS` is R2's "other branch" (the moving phase follows the rows), built, rows `yg3_mf_*`. R4 unchanged; MT rows on j3a barely engaged the tail half.

> **STATUS (2026-10-19, gcj j3/yg3): OPEN (perf / diagnostics; no wrong answer known).** Filed by lane yg3 with its fixes (`docs/internal/gc-round-gcj-20261019/j3-yg3-report.md`). Each item says what would close it.

## R1. Why the two deferred mirrors never left young is not settled

On the vi22-dr YY rows (`GcfF3yYoungPremiumProbe`, one Java thread, JIT-warm) the conc10 deferral deferred the same
two young user mirrors at all 52 scans (`young_mirrors_deferred=104`). By reading, a deferred mirror is reached through
`seed_mirror_pins_of_old_owners` -> `mark_edge_precise` (`gc/src/gen_heap.rs`), i.e. SIDE-marked, and the sweep's
promotion walk ages side-marked survivors (`promotion_walk_chunk`, the `age_bumps` push), so candidate (b) of the j2/yg2
page does not hold as written. The likeliest is (a): a conservative or unrewritable JIT word names the mirror (a
compiled frame's `Class` value, an `ldc` constant), which puts it in the root list anyway and pins it at every sweep.
The j3 fix (`CRATONVM_GEN_YOUNG_MIRROR_ROOT_WHEN_PROMOTABLE`) does not depend on the answer -- it removes the forced
sweep -- but if (a) holds, the mirror still never leaves young (it is pinned in place by the pinned copy too), and
its loader cannot unload while it lives. Closes with one row: `yg3_md_kill_dbg` (j3-yg3 report) names the two classes
(`CRATONVM_DBG_MIRRORPIN=1`); then `CRATONVM_DBG=gc-stats` `prp_kept_pinned` / `prp_conservative_vetoed` on the default
arm say whether a JIT word vetoes their promotion.

## R2. With precise-root promotion off the forced sweep is still every cycle

`CRATONVM_GEN_PRECISE_ROOT_PROMOTE=0` (or a pause under `CRATONVM_GEN_CONC_PAUSE_NO_PRECISE_ROOT_TABLE`) keeps the
conc10 widening (`vm/src/memory/roots.rs` `young_mirror_widens`), so the old behaviour returns there: every JIT-warm
young cycle swept in place for two young mirrors. Deliberate (a rooted young mirror would be pinned forever by the
sweep there), but it means the PRP kill switch now also kills the pinned copy on such programs. Closes when the
widening is retired outright (proposal `gcj-j3yg3-proposal-retire-the-young-mirror-deferral-20261019`), or when the
pinned-copy branch follows `mirror_pin` rows the way the licensed moving `System.gc()` does
(`side_table_young_rows_of` / `side_table_values_due`, `CRATONVM_GC_SYSTEM_GC_MOVING_SIDE_TABLES`).

## R3. The young geometry is designed, not armed

Part 1 of `gcj-j1yg-proposal-serial-young-geometry-and-no-pause-goal` (Serial eden inside `-Xmn`): the overflow
promotion and the planner are built (j3); the to-space copy cap, the semi reservation, the budget readers, the trigger
and the give-back are not (the proposal's step list). Until then `-Xmn` / `NewRatio` runs ~1.68x HotSpot's young
collections (j1/yg finding 5).

## R4. `-XX:MinTLABSize` still does not reach one floor

`CRATONVM_GEN_TYPED_MIN_TLAB_FLOOR` (j3) moves the smallest-whole refill and the tail sink to the typed value, and the
share sizer's desired size already follows it (`Tlab::share_refill_request_sized` passes `sizing.min_size` to
`desired_from_average_with`). Still on the 8 KiB constant: the early-retire refill cap
(`Tlab::early_retire_refill_cap`, `gc/src/tlab.rs`, `clamp(MIN_TLAB_SIZE, MAX_TLAB_SIZE)` and the
`last_refill_size.max(MIN_TLAB_SIZE)` above it), so with `-XX:MinTLABSize=2048` a thread that retires early after
using 512 bytes re-carves 8 KiB where HotSpot's floor would allow 2 KiB. Closes by clamping at the caller's
`sizing.min_size` (the function is called from `share_refill_request_sized`, which has it). LOW: only a typed value
below 8 KiB differs, and only on early retires.
