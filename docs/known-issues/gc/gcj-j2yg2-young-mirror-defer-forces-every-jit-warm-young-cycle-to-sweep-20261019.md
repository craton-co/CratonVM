# The young-mirror deferral forces every JIT-warm young cycle to the in-place sweep, and its mirrors never leave young

> **STATUS (2026-10-20, gcj j3/yg3, rows on `cratonvm-gcj-j3a`): OPEN -- the 2026-10-19 fix is REFUTED, a replacement is IN CODE behind an opt-in (PENDING BUILD/PROBE).** Rooting the young mirror (`CRATONVM_GEN_YOUNG_MIRROR_ROOT_WHEN_PROMOTABLE`) removed the forced sweep (`yg3_md_def`: `moving-pinned-pages=28` of 31) but kept dead loaders: every default `GenR5W5ConcUnloadProbe` / `GenR5W3ConcUnloadProbe` row printed `dead-loader-unloaded=false` (HotSpot `true`), every `=0` row unloaded at round 0 by a young cycle. The orchestrator returned that switch to opt-in (`6e5108c15c`). Replacement, commit `00a9037532`: `CRATONVM_GEN_YOUNG_MIRROR_DEFER_MOVING_FOLLOWS=1` keeps the deferral and makes step 14w' ask the moving young phase to follow the `mirror_pin` rows (the opt-in moving `System.gc()`'s gci i9/yl machinery, `side_table_follow` in `collect_garbage_inner_with_pins`) instead of forcing the sweep, so a dead young loader's mirror is still reached by nothing. Deciding rows `yg3_mf_*` (`docs/internal/gc-round-gcj-20261019/j3-yg3-report.md` section 0). Also from the rows: the two mirrors deferred per scan are not user-loader mirrors (no `roots: user class=` line on `yg3_md_kill_dbg`); they come through the non-strong hidden-class arm.

> **STATUS (2026-10-19, gcj j3/yg3): FIXED IN CODE, PENDING BUILD/PROBE -- `CRATONVM_GEN_YOUNG_MIRROR_ROOT_WHEN_PROMOTABLE` (default ON, `=0` kills), commit `ec4caf6a4c`.** The deferral's premise (a young user mirror must not be a root value because the in-place sweep pins every root value) was removed by two 2026-10-13 default flips: precise-root promotion (`CRATONVM_GEN_PRECISE_ROOT_PROMOTE`: the sweep no longer pins a value only precise tables name, and the class-mirror table is one of them, `vm/src/memory/roots.rs` step 6 = `precise_sections[2]`) and the pinned young copy (a term-4 cycle copies, pinning only the ledger's words). So step 6 of `collect_roots` no longer WIDENS the deferral to young mirrors on a scan that publishes the precise-table counts (`precise_root_promote` local, `young_mirror_widens`): the mirror is rooted from the table (the `CRATONVM_GEN_YOUNG_MIRROR_DEFER=0` root set for it, a superset, so no promise is dropped), `young_mirrors_deferred` stays 0 and step 14w' forces nothing; the cycle reaches term 4 and the pinned copy, and the mirror is copied, aged and promoted there or promoted by PRP in a sweep. With PRP off the conc10 deferral is kept (a rooted young mirror would be pinned forever there). HotSpot parity: Serial's young collection roots every loader's mirrors too; the one cost is that a dead YOUNG loader kept only by its young mirror unloads after that mirror's promotion. WHICH survivor rule kept the two mirrors young under the deferral is still not settled (candidates (a)-(c) below; `seed_mirror_pins_of_old_owners` side-marks them, `gc/src/gen_heap.rs` `mark_edge_precise`, so they do age -- (a), a conservative word naming the mirror, is the likeliest): the `yg3_md_kill_dbg` row names them. Rows `yg3_md_*` in `docs/internal/gc-round-gcj-20261019/j3-yg3-report.md` section 4; pass = this page's criterion below (stdout SAME, `young_mirrors_deferred=0` on the default arm, `moving-pinned-pages` or `moving` on >= 90 % of the JIT-triggered cycles) plus the conc10 regression rows (`GenR5W5ConcUnloadProbe` prints HotSpot's line).

> **STATUS (2026-10-19, gcj j2/yg2): OPEN (perf, HIGH for the evac7 residual) -- found by reading the base build's own shutdown summary.** No wrong answer is known. The evidence is the vi22-dr YY rows the orchestrator ran on `cratonvm-gcj-base.exe` for the `CRATONVM_GEN_TLAB_REFILL_SMALLEST_WHOLE_BLOCK` triage (`C:/craton/gcj-runs/triage/vi22-dr/dr22_yy_prem_{on,off}_r{1..7}.err`), all 14 runs alike.

## What the summary says

`GcfF3yYoungPremiumProbe 4000000 5` at `-XX:+UseGenerationalGC -Xmx512m`, ONE Java thread, JIT-warm:

```
[GC] decision histogram: moving=0 non_moving=55 skipped=0 nonmoving-coverage-incomplete=52 nonmoving-explicit-full-gc=3
[GC] moving_young_fallback_reason: none=49
[GC] moving_young_fallback_reason: unregistered-jit-frame-on-stack=3
[GC] young_pinned_copy: pycopy_cycles=0 ...
[GC] conc_y2o: ... young_mirror_defer_scans=52 young_mirrors_deferred=104 ...
```

(the off arm r1: 135 of 138 cycles `nonmoving-coverage-incomplete`, `none=133`, `young_mirror_defer_scans=135 young_mirrors_deferred=270`; every one of the 14 runs defers exactly 2 mirrors per scan.)

So every young cycle the JIT triggered (52 of 52; the other 3 are the probe's `System.gc()`) was diverted by TERM 3
(`divert_for_incomplete_moving_coverage`, `gc/src/gen_heap.rs` `collect_garbage_inner_with_pins`, the
`coverage_incomplete` / `divert_for_incomplete_moving_coverage` lets), on a single-threaded program, with no
recorded reason, and the pinned in-place copy (default ON) never ran.

## Why (by reading, today's tree)

1. `vm/src/memory/roots.rs` step 6 (`collect_roots`, the `young_mirror_defer` / `widened` lets in the class-mirror
   loop): `CRATONVM_GEN_YOUNG_MIRROR_DEFER` -- **default ON** (`young_mirror_defer_licensed` reads it with
   `runtime_flag_default_on`; `types/src/flag_groups.rs` row `gen-young-mirror-defer`, `off_word Some("0")`; the
   comments at both reads still say "opt-in") -- leaves a YOUNG user-loader mirror out of the root set and counts it in
   `young_mirrors_deferred`.
2. Step 14w' (same file, `if young_mirrors_deferred > 0`) then calls
   `cratonvm_gc::gc_quiescence::set_force_non_moving_jit_roots()`, which is `force_non_moving_jit_roots()` in the
   collector's term 3. That door records no `incomplete_reason`, so the decision reads `reason=none` (finding 4 below).
3. The deferral's premise (roots.rs, the `young_mirror_defer_licensed` doc): "it is no longer a root value, so it ages
   and is promoted like any heap-reachable survivor, and once old it is deferred by the heap's own rule". On this run
   it is not: the SAME count (2 mirrors per scan) is deferred at every one of 52 scans, across far more than the
   tenuring threshold's cycles (`gen_tenuring_threshold=15`, `young_age_clock_ticks=43`). The two mirrors never leave
   young, so the forced in-place promise is renewed at every cycle, for the life of the process.
4. Term 3 then also blocks the pinned copy (`term4_alone` needs `!divert_for_incomplete_moving_coverage`), as the
   step 14w' comment intends ("which also keeps the pinned copy out"), so a JIT-warm single-thread program never
   copies young: fragmentation (`gcg-g1c`), O(allocated) sweep pauses, and the smallest-whole refill's reason to exist.

Which survivor rule keeps the two mirrors young is NOT settled by reading. Candidates, each checkable with one row:
(a) the mirror is a conservative root value (a compiled frame or a compiled-code constant holds the `Class`), so the
selective-promotion walk counts it `census_pinned` (`gen_heap.rs` `promotion_walk_chunk`, the `is_pinned` test, and its serial twin) every
cycle; (b) its age is not bumped because it is marked only through a `mirror-old-owner` edge
(`seed_mirror_pins_of_old_owners`) and the aging list (`age_bumps`) takes header-unmarked or side-marked objects only;
(c) the promotion copies it and some side table still names the young address, re-deferring a stale entry.

## What would close it

* Run the probe row below with `CRATONVM_DBG_MIRRORPIN=1` (names the two classes and each cycle's decision) and
  `CRATONVM_DBG=gc-stats`; read `prp_kept_pinned` / `sp_pinned` against the two mirror addresses.
* Then either make the deferred mirrors promotable (for (a): a deferred mirror reached through its old loader's
  `mirror_pin` row is promoted on first sight, the way HotSpot treats a `Class` in eden; for (b): age it on the
  `mirror-old-owner` edge), or bound the promise: a mirror deferred for more than `MaxTenuringThreshold` cycles without
  leaving young is rooted again (the pre-conc10 behaviour for that mirror only), so term 3 stops firing for it.
* Label the door: `set_force_non_moving_jit_roots` callers should record a reason (a new
  `incomplete_reason::FORCED_IN_PLACE_PROMISE`, or one per caller: osr-shadow already has one; the 14w weak-mode
  promise and the 14w' mirror promise have none), so `moving_young_fallback_reason` stops reading `none`. Shared code
  (`gc/src/gc_quiescence.rs`, lane cm) and the root gatherer: a cross-lane hunk, not made here.
* Fix the two stale "opt-in" comments at the `young_mirror_defer_licensed` reads and the flag row's comment.

## Rows (battery format; `$GEN` = `-XX:+UseGenerationalGC`)

```
p yg2_mdefer_def   900 "CRATONVM_GC_STATS=1 CRATONVM_DBG_MIRRORPIN=1"                      "$GEN -Xmx512m" "GcfF3yYoungPremiumProbe 4000000 2" "-XX:+UseSerialGC -Xmx512m"
p yg2_mdefer_kill  900 "CRATONVM_GC_STATS=1 CRATONVM_GEN_YOUNG_MIRROR_DEFER=0"             "$GEN -Xmx512m" "GcfF3yYoungPremiumProbe 4000000 2" "-XX:+UseSerialGC -Xmx512m"
```

Pass for the closing fix: stdout SAME; on `yg2_mdefer_def` `young_mirrors_deferred` stops growing after at most
`MaxTenuringThreshold` scans, and the decision histogram shows `moving-pinned-pages` (or `moving`) on at least 90 % of
the JIT-triggered cycles. `yg2_mdefer_kill` is the control: with the deferral off the mirrors are roots (pinned
values), term 3 does not fire from 14w', and the cycles reach term 4 / the pinned copy -- if they do not, the cause is
elsewhere and this page is wrong about the mechanism.
