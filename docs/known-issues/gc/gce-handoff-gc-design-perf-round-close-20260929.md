# Handoff: the GC design and performance round closed after wave e2, verified on Linux

> **STATUS (2026-09-29, gce orchestrator): OPEN -- handoff.** The round stopped at the user's
> request ("no more waves") after two waves, both merged into `dev` and verified on Linux on
> Generational, G1 and ZGC. This page supersedes
> `gcd-handoff-gc-defects-round-close-20260929` (retired to `docs/internal/gc/` with this commit)
> and carries its open items. Retire it once every item below is done or moved to its own page.

## Where the round stands

- **Summary:** [`docs/internal/gc-design-perf-round-20260929/README.md`](../../internal/gc-design-perf-round-20260929/README.md)
  (what landed, the switch decisions, the battery numbers). Lane reports, row lists, rules and
  verdicts are in the same folder; the deciding file for every page is
  [`ve2-verdicts.md`](../../internal/gc-design-perf-round-20260929/ve2-verdicts.md).
- **Scope:** Serial / Generational and shared infrastructure; G1 and ZGC internals out of scope.
- **Binaries on the host** (`/data/gce-bin/`): `cratonvm-gce-base` (`adb9178bc`), `-e1`, `-e1b`,
  `-e1c`, `-e2` (wave e2 before the dev merge). Results: `/data/gce-out/`. Class dir for the
  e2 probes: `/data/wt-gce/bc-e2`. Runner: `CP=<classes> LIST=<list> /data/wt-gce/battery2.sh <bin> <out> [filter]`.
  **Always set `CP`**: without it every row fails to load its class and both sides print
  nothing, which reads as SAME.

## Defaults changed at the close

Decided on `cratonvm-gce-e2` with the environment set (ve2 plus a full default battery per
candidate), then flipped in source: `CRATONVM_XT_TAKEOVER_SIGNAL_JIT_ONLY` (on),
`CRATONVM_XT_FIRST_PASS_GRACE_US` (200 us), and the conc-unload four
(`CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK`, `CRATONVM_GEN_CONC_CLASS_UNLOAD`,
`CRATONVM_GEN_Y2O_LIVE_SEED`, `CRATONVM_GEN_YOUNG_MIRROR_DEFER`); each has `=0` as its kill
switch. The merged tree (these defaults compiled in, plus dev's JIT round 14 at `225179022`)
was built on Linux as `cratonvm-gce-close` and ran the default battery
(`/data/gce-out/battery-close`): 92 SAME / 24 DIFF, no roster hole, no crash. Against the e2
batteries it gained `w3_unload_*`, `w1_orphans_2`, `w3_jitoomroot_live` and lost three
`w2_jitoomroot_*` variants (`oome-thread-exit`); a 32-run A/B of that probe
(`/data/gce-out/cab/`: the close binary, with each flip set killed, and e2) gave no
`oome-thread-exit` failure at all, so those are the load-dependent OOME family of item 2.
The three `rc=124` rows (`tail_sink`, `tail_sink_off`, `w1_allocrate_2`) time out on every
binary since e1.

## What to do next, in order

1. **A default-flags SIGSEGV at OSR entry**
   ([`gce-x-osr-entry-holds-snapshot-reads-a-dead-local-in-a-decommitted-young-span-20260929.md`](gce-x-osr-entry-holds-snapshot-reads-a-dead-local-in-a-decommitted-young-span-20260929.md)):
   `MtChurnProbe 4 60 48` crashes 1-2 in 20 on every binary including the round's base;
   `OsrEntryHolds::snapshot` reads the mark word of a liveness-dead local whose young span
   was decommitted. The pinned-copy gate's MtChurn crash (ve2 y-1, 2/2) is the same pc, so
   fix this before judging `gengc-r4w6-pinstale6-pinned-copy-default-flip-gate` again.
2. **Load-dependent OOME retention** (`gcd-d6u-pinned-callee-oome-heap-stays-full-after-the-catch`,
   `gce-e2j-zgc-default-jni-cost-probe-oome-flake`, `jit_oom_root_orphans`' `oome-compiled-callee`):
   the three share the shape `chainCatch cleared=false usable=false` / an OOME after the
   catch, and reproduce on the base binary as often as on e2 (the A/B table at the end of
   `ve2-verdicts.md`: pcallee base 0/8, e2 0/8 under host load; jnicost ZGC base 5/8 failing).
   One holder is likely behind all three; census `CRATONVM_DBG=oldmark-root-census` on a
   failing pcallee run first.
3. **d9d argspecial attribution is inverted** (`gcd-d9d-call-argument-copies-still-rooted-on-other-call-shapes`):
   ve2 f-7 shows the single-pass local-map claim (`CRATONVM_GC_SP_LOCAL_MAP_ROOTS`), not the
   deopt residue claim, is what makes `argspecial` pass (`_noloc` fails 3/3 on every
   collector; `_nores` fails only Generational `private-special`). Row 8's `deadspill` did not
   rise (1422 on base and e2). Correct the page's attribution; d4o's DONE proposal retires with it.
4. **Not-entrant re-dispatch** (`gcd-d10f-not-entrant-redispatch-exception-reruns-the-callee-handler`):
   only 2 counted runs (Gen 1, ZGC 1) of the 3 per collector the page wants; rerun
   `f_<gc>_notentrant` with 10 reps. The control never re-dispatched either, so the rows need a
   warmer call site before they decide.
5. **NGR frame half** (`gcd-d10f-native-growth-reclaim-osr-main-holder-unnamed`): Generational 3/3,
   ZGC 2/2 in HotSpot's geometry; G1 0/2 is `gcd-d10v`. Retire the frame half if G1 is
   accepted as the G1 page's; the fragmentation half is `gce-e2f-old-humongous-compaction-refused-while-compiled-frames-are-live`.
6. **Undecided switches**: `CRATONVM_GEN_CONC_INLINE_START` (c-5: taken 3/3 but no completed
   cycles on either arm -- needs a probe where the concurrent cycle completes);
   `CRATONVM_GC_REFILL_TRIGGER_UNJUDGED` (o-6: the arm never engaged; `gcd-d9b-compiled-refill-trigger-*`
   needs a row that reaches the refill trigger); cards4 (y-6: re-measure with 5 pairs and the
   `post-barrier-needed` trace fixed); adaptive tenuring (y-7: promoted bytes flat).
7. **JNI package** (`CRATONVM_JNI_NATIVE_TRANSITIONS`): Generational P/A is near 2; G1/ZGC still
   spend 3-4 us in `dep_jit_scan`. Lane j's census rows (`j_*_census_P`) are in `/data/gce-out/ve2/`;
   the `frame_fallback_*` / `a5_*` counts decide between its two hypotheses (`e1-j-report.md` §10).
8. **Host-only rows** never placed: `VthreadGcStress` (not in the tree), Netty ByteBuf ON x10,
   QDox under gc-stress, Spring Boot ON / T1 (they gate pin8, pinwords5 and adaptive tenuring),
   the WildFly audit for cards4, e1-j §9's `taskset` experiment.
9. **Carried from the d10 handoff:** G1 items (out of scope: `gcd-d10v`, G1
   `local-catch-interface`); the `oomjit5` page can be re-filed as a known gap (ve2 f-9 passed
   on all three collectors) once ve1's osrorphan rows are re-read.

## Test-suite state at the merge

Green on Windows except four failures that are not this round's:
- `cratonvm-gc --test g1_w6p_mixed_parallel_equivalence` ("the two drivers disagreed about
  which objects to evacuate for 1784 of 6656 nodes"): intermittent, and a clean `origin/dev`
  checkout (`570ae47a5`) fails it 3 of 4 runs; G1, out of scope.
- `cratonvm-vm --test jit_bridge_sink_rerun_off_arm`: with `CRATONVM_JIT_DEOPT_SINK_RESUME=0`
  the round-12 replay gate (`f5f63cb78`, 2026-09-26, in the base) refuses the optimizing body
  of `DeoptRerunCount.hot` ("the deopt at bci 14 can only be replayed from entry ... keeping
  the single-pass body"), so the test's warm-up never sees an IR body. The test predates that
  gate; it needs `CRATONVM_C2_ACCEPT`-style override for the replay gate or a new witness.
- `cratonvm-types --test unverified_records::the_known_issues_root_exists`: `docs/known-issues/jdk-only`
  was removed by `778de74c4` ("bug docs update"); failing on `dev` before this round.
- `cratonvm-gc --test g1_doc_blocks_are_not_stranded`: a stranded doc block at `gc/src/g1.rs:16764`
  from `461272b37` ("save g1 work", on `dev`); G1 is out of scope.

The merge fixed two dev-side reds it met: `jit_bridge.rs`'s wave-42 diagnostic moved above
the `requires_wrapped_entry` stamp (the publication witness), and a newline-typed literal in
lane t's test.

## Pages filed by this round

Open: the `gce-e1*` and `gce-e2*` pages in this folder (defects: census names, in-native
deposit rescans, G1/ZGC collection time, humongous compaction refused, uncaught report order,
ZGC jnicost flake, first-pass grace; proposals: one-pass safepoint facts, refill slow path,
parallel take-over signals, helper-window pass skips peers).
