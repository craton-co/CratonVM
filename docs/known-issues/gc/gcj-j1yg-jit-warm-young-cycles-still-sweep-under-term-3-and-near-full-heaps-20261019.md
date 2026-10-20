# JIT-warm young cycles still sweep in place under term 3 and on near-full heaps

> **STATUS (2026-10-20, gcj j3/yg3): OPEN; the line below is superseded.** The fourth source is NOT fixed on the default path: the rooting fix was refuted on j3a (dead loaders kept) and is opt-in again. The opt-in replacement `CRATONVM_GEN_YOUNG_MIRROR_DEFER_MOVING_FOLLOWS` (rows `yg3_mf_*`, j3-yg3 report section 0) is what would remove it. On j3a the pinned copy that replaced the forced sweep was not cheaper in wall time on `GcfF3yYoungPremiumProbe` (medians 7634 vs 6192 ms, 3 reps), so this page's close criterion needs the CPU A/B too.

> **STATUS (2026-10-19, gcj j3/yg3): OPEN, the j2/yg2 fourth source FIXED IN CODE (PENDING BUILD/PROBE).** Step 14w' no longer forces term 3 on a scan that publishes precise-table counts (`CRATONVM_GEN_YOUNG_MIRROR_ROOT_WHEN_PROMOTABLE`, default ON; `gcj-j2yg2-young-mirror-defer-...` page), so a single-threaded JIT-warm run's term-4 cycles reach the pinned copy again. What remains on this page: item 1's multi-thread sources (cross-thread JIT peers, helper windows: the take-over arm `CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER`, lane pn), the 14w weak-mode promise (now labelled `forced-in-place-side-table-promise`, fires only when term 4 is not armed), item 2's promotion-risk / explicit / GPU terms, item 3's declines and item 4's copy cost. New from this lane: once the pinned copy runs on these cycles, its rebuild's vacated spans are quarantine-poisoned and listed, so every refill from them is memset (the j2/yg2 known-zero carry covers sweep cycles only): `gcj-j2yg2-proposal-young-free-memory-is-always-zero` names the rebuild arm as the real work. Rows `yg3_md_*` (j3-yg3 report) re-measure the single-thread share; the close criterion is unchanged.

> **STATUS (2026-10-19, gcj j2/yg2): OPEN, a new largest cause found -- `docs/known-issues/gc/gcj-j2yg2-young-mirror-defer-forces-every-jit-warm-young-cycle-to-sweep-20261019.md`.** Term 3 is not only a multi-thread condition: on the single-threaded `GcfF3yYoungPremiumProbe` (vi22-dr YY, 14 runs on the base build) every JIT-triggered young cycle (52/52, 135/135) is `nonmoving-coverage-incomplete` with `moving_young_fallback_reason: none`, because step 14w' of `vm/src/memory/roots.rs` (`CRATONVM_GEN_YOUNG_MIRROR_DEFER`, default ON) defers two young user-loader mirrors at every scan and forces the in-place promise through `set_force_non_moving_jit_roots` (a door that labels no reason), and the two mirrors never leave young (`young_mirrors_deferred` = 2 x scans). So item 1 below has a fourth source, the pinned copy never runs on such a program (`pycopy_cycles=0`), and the close criterion cannot be met until that page is fixed. Nothing else on this page changed; no code here (the root gatherer and `gc_quiescence.rs` are other lanes'). The j2/yg2 known-zero carry (`CRATONVM_GEN_YOUNG_KNOWN_ZERO_CARRY`) lowers the sweep's refill memset cost on exactly these cycles; it does not make them copy.

> **STATUS (2026-10-19, gcj j1/yg): OPEN (perf) -- the residual of `docs/internal/gc/gengc-r5w3-evac7-jit-warm-young-cycles-sweep-in-place-and-fragment-young-RETIRED-UNFIXED-20261018.md` after its gcj j1/yg narrowing.** Nothing here is a wrong answer. HotSpot Serial copies every young collection; on a JIT-warm Generational heap these cycles run `sweep_young_non_moving` instead, which fragments young (`gcg-g1c-...`) and whose pause is O(allocated).

## What still sends a JIT-warm cycle to the sweep (`gc/src/gen_heap.rs` `collect_garbage_inner_with_pins`)

1. **Term 3, `divert_for_incomplete_moving_coverage`** -- `takeover_forbids_unpinnable_move()`, a frozen peer or a
   helper window, or `moving_young_coverage_incomplete()` (`cross-thread-jit-peer`). vi4-pn measured it on EVERY
   cycle of 4-thread javac (226/226) and of the NIO echo rows (180/180). The pinned copy takes such a cycle only
   through its take-over arm, opt-in `CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER` (lane pn's code and rows).
2. **Term 4 with a second divert term** -- now counted by `[GC] young_term4_offer:` (gcj j1/yg). The promotion-OOM
   guard's share is offered to the pinned copy under the opt-in
   `CRATONVM_GEN_PINNED_COPY_TAKES_PROMOTION_RISK`; the GPU veto and `System.gc()` (`explicit_full_gc`, a root-gatherer
   contract, see the TERM comment at the `explicit_full_gc` read) are not.
3. **The pinned copy's own declines** -- `pycopy_ledger_incomplete` (a peer did not deposit), `pycopy_over_bound`
   (more than 1/8 of from-space's pages pinned).
4. **Cost, not a divert:** where the copy runs, its pause median is above the sweep's (ve2 y-4: 91.5 vs 66.8 ms on
   `GenR4W4EvacThroughputProbe`): the per-survivor `pointer_map` insert (pin10 item 5) and the in-place rebuild.

## What would close it

(1) is the take-over arm's flip plus the helper-window licence (lane pn / cm); (2) is the opt-in's flip after rows
`yg1_t4o_*` (`docs/internal/gc-round-gcj-20261019/j1-yg-report.md`); (4) is pin10 item 5, measure-first. The page
closes when a JIT-warm `GenR4W4EvacThroughputProbe` and a 4-thread javac row show `moving` or `moving-pinned-pages`
on at least 90 % of their young cycles with the young-pause median within 1.25x of HotSpot Serial's `-Xlog:gc`
median, CPU time compared interleaved.
