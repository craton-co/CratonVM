# The generational concurrent cycle can clear references but cannot unload a class loader

> **STATUS (2026-09-29, gce ve2): OPEN -- the four conc-unload switches are now default on (ve2 c-4 and the four-switch battery pass); still unrun: the gc-stress set and the Tomcat undeploy census.** Evidence: `docs/internal/gc-design-perf-round-20260929/ve2-verdicts.md`; next steps: `gce-handoff-gc-design-perf-round-close-20260929.md`.

> **STATUS (2026-09-29, gce e1/x): KEEP -- gate item 2's failing token now passes; the rest of the four-switch gate is unrun.** `remarkrefs_hook_1..3` (`verify-e1/ve1`) = HotSpot 3/3 on e1 (base 0/3). **Remaining:** gate items with all four switches (full battery, gc-stress set, Tomcat undeploy census), then the flip.

> **STATUS (2026-09-29, gce e1/c): gate item 2's failing token ADDRESSED by a cross-lane hunk (pending) -- the four switches then need no fifth.** `weak-to-resurrected-cleared=false` is the live-finalizable pin (`gcd-d9e-non-moving-sweep-pins-every-live-finalizable-young-20260928`); d9/e's opt-in fixes it (measured 3/3) but was a separate switch. Request 1 of `docs/internal/gc-design-perf-round-20260929/e1-c-report.md` makes the non-moving young sweep promote live finalizables whenever `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK` is on (default path unchanged). Re-run item 2 with `$F4` only after it lands. Also found in this review and fixed (default path; `gcd-d9a-moving-major-misses-side-table-rows-of-moved-owners-20260928`): a moving cycle's Phase 5 major could free the old mirror / metadata values of a user loader moved in that pause, and a pinned loader promoted in that pause; both bear on items 1 and 5 whenever `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG` is set.

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`, wave d7, Linux release): unchanged -- mechanism complete behind the four opt-ins; the default still cannot unload concurrently; the flip gate is partly run and item 2 FAILS one token.**
>
> - **Default:** `GenR5W3ConcUnloadProbe` prints `dead-loader-unloaded=false dead-class-unloaded=false` (HotSpot `true`) under gc-stress (`c10s_unload_1..3`), with two switches (`u7_*`), and in 5 of 10 runs with the share sizer (`s10_share_unload_*`, the other 5 unload).
> - **Gate item 1 (partial):** the four switches with `CRATONVM_GC_CONC_START_PERCENT=40` on `GenR5W5ConcUnloadProbe` (`four_flags_unload_1..3`): HotSpot's line 3/3, no exception, `concunload_layouts_retained=1 concunload_layouts_released=1`, `y2o_live_pauses=10`. Not run: 5/5 per arm, the arm without the start percent, `--nojit`, `GenR5W3ConcUnloadProbe`. The mirror switch alone no longer throws (`mirror_defer_only_1..3`, HotSpot's line 3/3).
> - **Gate item 2:** `GenR5W5RemarkRefsProbe` still prints `weak-to-resurrected-cleared=false` with the hook and the live young seed (`mark2_remarkrefs`; see the mark2 page).
> - **Remaining gate:** items 1-6 as written below; item 2's token first.

*Filed 2026-09-26 by gen round 5 wave 1, lane `refs5`: the residue of
`gengc-mark2-gen-concurrent-cycle-has-no-remark-reference-processing-FIXED-20260929.md`
after its reference half landed (opt-in).*

## STATUS (2026-09-28, gcd d5/r): mechanism COMPLETE behind four opt-ins and measured (HotSpot's line 3/3 on the d1 build); the EXACT flip gate of the four switches is below; no code change

**The four switches** (all off by default; flip them TOGETHER, never one alone -- the triage's d1 run showed
`CRATONVM_GEN_YOUNG_MIRROR_DEFER` alone exposed a default-path hole, fixed
by d1/c):

| Switch | What it adds |
|---|---|
| `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK` | reference processing at the concurrent remark (`gen_remark_process_references`) |
| `CRATONVM_GEN_CONC_CLASS_UNLOAD` | class unloading at the remark (`gen_remark_unload_classes`), layouts retained for the sweep's census |
| `CRATONVM_GEN_Y2O_LIVE_SEED` | the concurrent pauses seed old from the LIVE young set, and hide young `Reference`s' old referents |
| `CRATONVM_GEN_YOUNG_MIRROR_DEFER` | young user mirrors deferred on licensed scans, so they age out of young under the JIT |

**Why they are not default yet (the triage):** the defect the mirror switch
exposed (a promoted mirror, the STW old collection missing the young
instance -> loader edge) was fixed only in d1/c, so the four switches have
never had a full battery nor a real-workload census on a build with that
fix. Nothing in d2-d5 changed their code; d4/n's true-root seed and d5/r's
fix to it are stop-the-world only and apply with the switches on or off.

**The gate** (Linux release, JIT on unless stated; one binary; every
stdout compared with its no-flag run and with `java -XX:+UseSerialGC` at the
same `-Xmx` where the probe prints HotSpot-comparable lines):
```
F4="CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1 CRATONVM_GEN_CONC_CLASS_UNLOAD=1 CRATONVM_GEN_Y2O_LIVE_SEED=1 CRATONVM_GEN_YOUNG_MIRROR_DEFER=1"
P="--java-home $JDK -XX:+UseGenerationalGC -cp tools/bench"
```
1. **Unloading, 5/5 each, `-Xmx256m`, with and without
   `CRATONVM_GC_CONC_START_PERCENT=40`, and once `--nojit`:**
   `env $F4 CRATONVM_DBG=gc-stats cratonvm $P -Xmx256m GenR5W5ConcUnloadProbe`
   and the same for `GenR5W3ConcUnloadProbe`. Stdout
   `conc-unload dead-loader-unloaded=true dead-class-unloaded=true control-loader-alive=true control-ok=true`
   (HotSpot's line) and never an exception; stderr `concunload_classes>=1`,
   and when `concunload_layouts_retained>=1` also
   `concunload_layouts_released>=1` with released <= retained (the
   retained-layout page). A `NullPointerException` on `controlHolder` is the
   d1/c hole back: stop.
2. **References, 3/3, both `CRATONVM_SOFTREF_HOTSPOT_LRU` modes for the soft
   probes:** `GenR5W5RemarkRefsProbe -Xmx256m` (HotSpot's
   `remark-refs ... weak-to-resurrected-cleared=true ...` line, see the mark2
   page), `GenR5W1RemarkRefProcProbe`, `GenR4RefClearProbe`,
   `GenR4WeakToFinalizableProbe` (`PASS`), `GenR4DroppedReferenceLeakProbe`,
   `GenR4SoftRefLruProbe -Xmx256m`, `GenR5W1SoftRefYoungProbe`,
   `GenR5W2SoftRefIdleYoungProbe`: stdout equal to the no-flag run (or
   HotSpot's where they differ only because the no-flag arm processes
   nothing at a remark).
3. **Concurrent-cycle correctness, 3/3:** `GenR4W4SteadyPromotionProbe 4
   -Xmx512m` (`checksum=558000555608192`, `concdrv_cycles_completed` not
   below the no-flag run), `GenR4W6ConcCadenceProbe`,
   `GenR4W5ConcMarkJitGateProbe`, `GenR4W3GcNotificationProbe` (a remark adds
   no notification). `[GC] conc_y2o:` fallback counters below the pause
   count.
4. **Tests:** `cargo test -j 5 -p cratonvm-gc --lib` (the `r5w1_`, `r5w3_`,
   `r5w4_`, `y2o_live`, `gcd_d1c_`, `gcd_d2g_` tests), `cargo test -j 5 -p
   cratonvm-vm --lib` (`roots::`, `memory::gc::`, `native_roots`,
   `r5w6_young_mirror_defer_rule_truth_table`), and the `vm/tests` suites
   `class_loader_unload_regression`, `wave2_cleaner`,
   `jit_alloc_oome_clears_soft_refs`, `stackwalker_reflect_gc`,
   `g1_class_unloading_wired`; `gc/tests/wp1_10_reference.rs`,
   `gc/tests/gengc_r4w2_concmark_cycle_ownership.rs`. Run the two suites
   that read flags with `$F4` in the environment as well.
5. **Real workloads (Linux host), `$F4` vs no flags:** netty
   `io.netty.buffer` x4 (zero SIGSEGV, same pass count); Tomcat
   `TestDefaultInstanceManager` (`testClassUnloading`: 8 cached annotation
   entries) and the Tomcat census, the census's unloaded-class count not
   below the no-flag run; the Spring Boot sample (exploded): startup, one
   request, clean exit.
6. **gc-stress:** `CRATONVM_DBG_GC_STRESS=65536` with `$F4` on
   `GenR4W4SteadyPromotionProbe 4`, `GenR5W3ConcUnloadProbe`,
   `GenR4W4HeapFullThrashProbe -Xmx64m` (its usual pass rate on the base,
   both arms): no fault, no verifier anomaly.
7. **Y2O inventory (the Y2O page's gate):** confirm by grep that no VM table
   hands back a young object without being a root or feeding the live
   seed's `extra_live`: today the JNI weak globals (fed) and the finalizable
   list (fed). Any new such table found blocks the flip of
   `CRATONVM_GEN_Y2O_LIVE_SEED` (and the other three with it).
8. **Mode invariance:** item 1 and `GenR5W5RemarkRefsProbe` also under
   `--compatible` (the hook changes WHEN references are cleared, which
   `--compatible` programs can observe: record the new timing as HotSpot's
   in `docs/GC.md` with the flip).

**Flip:** `gen_conc_remark_refproc_hook` and `gen_conc_class_unload` in
`types/src/flags.rs` from `non_empty_non_zero` to `on_unless_zero` (and the
`types` default assertions for them inverted); the other two are read
through `cratonvm_types::flags::runtime_flag_on` -- `Y2O_LIVE_SEED_FLAG`
(`gc/src/gen_heap_y2o_live.rs`), `YOUNG_MIRROR_DEFER_FLAG`
(`vm/src/memory/roots.rs`) and the census-line gate in
`gc/src/concurrent_mark.rs::y2o_census_line`
(`runtime_flag_on("CRATONVM_GEN_YOUNG_MIRROR_DEFER")`, diagnostics only)
-- and move to `runtime_flag_default_on` (d4/n's pattern), with
`off_word: Some("0")` on their `types/src/flag_groups.rs` entries. `=0` on
any one restores that part (name all four `=0` switches in `docs/GC.md`). Pages that then retire together: this
one, `gengc-mark2-...`, `gengc-r4w5-concmark5-remark-reference-processing-design-...`,
`gengc-r5w3-unload7-retained-layouts-are-never-released-...` (if item 1
showed a release), `gengc-r5w6-conc10-a-young-mirror-root-is-pinned-forever-...`,
`gengc-r5w6-conc10-young-to-old-seeding-keeps-dead-old-objects-...`
(concurrent half). Owner: orchestrator.

## ORCHESTRATOR RUNS (2026-09-27, round-5 tip `c6c98c760`, Linux release, JIT on): conc10's two opt-ins UNLOAD the dead loader

These runs use `GenR5W5ConcUnloadProbe -Xmx256m` with
`CU="CRATONVM_GEN_CONC_CLASS_UNLOAD=1 CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1 CRATONVM_GC_CONC_START_PERCENT=40"`.

| Arm | Runs | Line printed |
|---|---|---|
| `$CU` + `CRATONVM_GEN_Y2O_LIVE_SEED=1` + `CRATONVM_GEN_YOUNG_MIRROR_DEFER=1` | 3/3 | `conc-unload dead-loader-unloaded=true dead-class-unloaded=true control-loader-alive=true control-ok=true` |
| `$CU` alone | 2/2 | `dead-loader-unloaded=false dead-class-unloaded=false`, the wave-5 result |
| Default flags | 2/2 | `dead-loader-unloaded=false dead-class-unloaded=false`, the wave-5 result |

The first arm prints HotSpot Serial's line, and with the JIT on. On the
wave-5 build only `--nojit` did that.

It stays opt-in. The default flip of the four switches together is a user
decision; it needs the battery with all four on, and a gc-stress run.

## STATUS (2026-09-27, gen r5w6/conc10): the JIT-only retention EXPLAINED (young holders pinned by the root-value pin, seeded strongly); two opt-in fixes IMPLEMENTED (`CRATONVM_GEN_Y2O_LIVE_SEED`, `CRATONVM_GEN_YOUNG_MIRROR_DEFER`); unbuilt

**The wave-5 measurement.** `GenR5W5ConcUnloadProbe` unloads with `--nojit`
(HotSpot's line, 5 remarks) and not with the JIT (63 remarks), every remark
saying `loader=...(old=true marked=true roots=young_to_old+young_instance_loaders)`
and the dead `Payload` mirror `old=false roots=collect_roots`, plus
`[MIRRORWHY] root via FRAME class="...$Payload"` (6x).

**What that is** (read; details on the two new pages):
- The FRAME line is the CONTROL class's interpreter frame on `main`
  (`step()` → `Payload.get()` / `<init>`); it pushes the control loader,
  correctly. Not a JIT holder: no cross-lane request.
- The dead mirror is rooted by `collect_roots` step 6 (a YOUNG user mirror is
  deferred only when the young marker is certain to follow `mirror_pin`, which
  under moving-young is only `System.gc()`), and a root value is PINNED by the
  non-moving sweep's selective promotion, so under the JIT the mirror is never
  promoted; its `classLoader` field then seeds the loader at every pause
  (`gengc-r5w6-conc10-a-young-mirror-root-is-pinned-forever-FIXED-20260929.md`).
- The same pin keeps `deadLoaderRef` (a static's `WeakReference`) young, and
  a young `Reference`'s slot 0 is an ordinary young→old seed, i.e. the dead
  loader is seeded STRONGLY through its own weak reference; and
  `young_instance_loaders` names the loader of any young object, dead or not
  (`gengc-r5w6-conc10-young-to-old-seeding-keeps-dead-old-objects-FIXED-20260929.md`).
- With `--nojit` the young collection copies, all of these are promoted or
  collected, and the cycle unloads — as measured.

**What landed.** `CRATONVM_GEN_YOUNG_MIRROR_DEFER` (young user mirrors deferred
on licensed young-collection scans, the young cycle forced in place when one
was, so the mirror ages out of young); `CRATONVM_GEN_Y2O_LIVE_SEED` (the
concurrent pauses seed from the live young set, the live young objects'
loaders only, and — with the refproc hook — not through a young `Reference`'s
slot 0). Both opt-in; flag off is byte-for-byte the previous driver. Also,
default-on and independent: the concurrent seed now word-scans unparseable
young stretches, and the class unload roots every loader when the young walk
was incomplete (`../../internal/gc/gengc-r5w6-conc10-concurrent-young-to-old-seeding-drops-unparseable-young-stretches-FIXED-20260928.md`).
A new diagnostic line under `CRATONVM_DBG_MIRRORPIN_WHY`:
`[conc-unload-why] pause=P young-instances loader=0x.. classes=[<class id>x<count>:live|dead|all-young, ...]`.

**Run (orchestrator), `-Xmx256m`, JIT on:**

```
java -XX:+UseSerialGC -Xmx256m -cp tools/bench GenR5W5ConcUnloadProbe
for arm in "CRATONVM_GEN_Y2O_LIVE_SEED=1 CRATONVM_GEN_YOUNG_MIRROR_DEFER=1" \
           "CRATONVM_GEN_YOUNG_MIRROR_DEFER=1" "CRATONVM_GEN_Y2O_LIVE_SEED=1"; do
  env $arm CRATONVM_GEN_CONC_CLASS_UNLOAD=1 CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1 \
    CRATONVM_GC_CONC_START_PERCENT=40 CRATONVM_DBG=gc-stats CRATONVM_DBG_MIRRORPIN_WHY=1 \
    cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -Xlog:class+unload \
    -cp tools/bench GenR5W5ConcUnloadProbe
done
```

- Both flags: stdout `conc-unload dead-loader-unloaded=true dead-class-unloaded=true control-loader-alive=true control-ok=true`
  (= HotSpot); `old_collections_at_unload=0`; `[GC] conc_unload: ... concunload_loaders>=1 concunload_classes>=1`;
  `[GC] conc_y2o: ... y2o_live_pauses>=1 ... young_mirrors_deferred>=1`;
  one `unloading class GenR5W5ConcUnloadProbe$Payload` line; the late
  `[conc-unload-why]` lines show the dead mirror `old=true`.
- Mirror flag alone / Y2O flag alone: expected `dead-loader-unloaded=false`
  (each removes some of the holders); record which `roots=` remain — they
  confirm or refute the reading above holder by holder.
- If both flags still fail: the `[conc-unload-why] ... young-instances` line
  says whether a LIVE young instance of the dead class exists (a
  conservative-root keep, the JIT lane's family), and `roots=` names what is
  left.

Same arms on `GenR5W3ConcUnloadProbe` (its `drive()` frame is main's: expect
the same verdict or a JIT-root holder in `roots=`).

## Previous STATUS (2026-09-27, gen r5w5/conc9, superseded above): end-to-end trace done; no VM table roots a dead user loader under the licence; the leading suspect is the PROBE's own `get()` in a compiled frame; probes reworked + a per-loader "why live" report; unbuilt

**The measurement to explain** (orchestrator, wave 4 final build): with
`CRATONVM_GEN_CONC_CLASS_UNLOAD=1` the cycle opened and its remark ran the
unload (`concunload_remarks=1`) and unloaded nothing (`concunload_classes=0`),
and the later old-generation collections (STW majors: after the first cycle
the adaptive start is ~72 %, and one 34 MiB ballast step jumps from below it
to above the 75 % floor) did not unload DEAD either, while HotSpot Serial does.
The STW major failing too says the loader is REACHABLE (or pinned young), not
that the concurrent marker's side-table logic is wrong.

**The trace, stage by stage** (read, not run):

1. *Roots under the licence.* Every `collect_roots` step and every
   `native_roots` row was read for a user loader or mirror pushed outright
   under `conditional_metadata`: none. `defining_loader_store`,
   `loader_namespace_id_store` and `loader_meta_store` hold strong
   `ObjectRef`s but are not roots when loader unloading is on; the unnamed
   `Module` lives in the loader's own field; `getDeclaredConstructors0` caches
   nothing; `reflectionData` is a class-atomic slot deferred through
   `defer_or_root` (which already uses `vm_deferral_owner`); monitors are not
   roots except through frames and the per-thread JMX lock stack.
2. *Per-thread roots of the thread that defined DEAD* — in W3 that is `main`,
   the thread whose compiled `drive()` loop then drives every cycle:
   - **W3's round loop called `deadLoaderRef.get() == null` every round.**
     Until DEAD is cleared that puts the DEAD loader and class into the OSR
     frame of `drive()`, and the conservative JIT root scan (and the frozen-peer
     scan) roots and pins such a dead register/spill word. HotSpot's precise
     oop maps do not. This is the leading suspect; it is the JIT
     dead-reference family (`gengc-r5w2-oomjit6-ir-frames-keep-dead-references-as-roots`),
     not a class-unloading defect.
   - stale words of `define()`'s native frames under the compiled frames;
   - the JMX lock stack (`synchronized(this)` on the loader, re-entered from
     `defineClass`'s supertype resolution) if a retract is missed;
   - the initiator's root snapshot and activation records.
3. *Eligibility.* The remark's verdict is `remark_is_marked` = marked OR not an
   initial-mark old object start. A loader that is still YOUNG (pinned by a
   conservative root, so never promoted) is live to every concurrent cycle by
   construction (G1 treats young the same way).
4. *Stale side-table keys* (`mark_rows_of_live_owners` treats a key that is not
   an eligible old object start as live and marks its mirrors, whose
   `classLoader` field then marks the loader): `mirror_pin` and `loader_pin`
   are rebuilt from the remapped defining-loader table after every STW pause,
   `metadata_pin` at every root scan; stale only if a move escaped the pointer
   map. Not found.
5. *Unload step.* Runs after `remark_finish` when authorised
   (`gen_remark_unload_classes`), judges every `defining_loader_store` row by
   `remark_is_marked`, unloads `unloads_on_hint` classes (user namespace or
   hidden). DEAD's class is a user-namespace class. Nothing found that would
   skip a dead row.

**What landed (this wave).**
- `tools/bench/GenR5W3ConcUnloadProbe.java`: the round loop and the verdict
  use `refersTo(null)` instead of `get() == null` (no referent in a compiled
  frame; no SATB keep-alive either).
- `tools/bench/GenR5W5ConcUnloadProbe.java` (new): the same workload with every
  DEAD reference confined to a SETUP thread that exits before the first cycle
  (its stack, snapshot and lock stack go with it); `main` never holds DEAD.
- A per-loader report under `CRATONVM_DBG_MIRRORPIN_WHY=1` (existing debug
  flag), `gc_and_alloc.rs::gen_conc_unload_why`: at the initial mark and at the
  remark (final bitmap, just before the unload), one line per user loader
  (`mirror_pin` owner):
  `[conc-unload-why] pause=<initial-mark|remark> loader=0x..(old=.. marked=.. live_or_ineligible=.. roots=<sources>) mirrors=[0x..(... class=...)]`,
  where `roots=` names which of `collect_roots`, `thread_snapshots`,
  `frozen_peers`, `young_to_old`, `young_instance_loaders` (and at the remark
  `refproc_keep`) named the object DIRECTLY (`-` = reached by the trace or
  ineligible). `old=false` or `live_or_ineligible=true` with `marked=false`
  means "young": item 3.
- `-Xlog:class+unload` and JFR `jdk.ClassUnload` per unloaded class
  (`memory::gc::report_unloaded_classes`; the unload7 proposal), so a run shows
  WHICH class went and by which collection.

**Expected (orchestrator):**
```
java -XX:+UseSerialGC -Xmx256m -cp tools/bench GenR5W5ConcUnloadProbe
java -XX:+UseSerialGC -Xmx256m -cp tools/bench GenR5W3ConcUnloadProbe
for p in GenR5W5ConcUnloadProbe GenR5W3ConcUnloadProbe; do
  CRATONVM_GEN_CONC_CLASS_UNLOAD=1 CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1 \
  CRATONVM_GC_CONC_START_PERCENT=40 CRATONVM_DBG=gc-stats CRATONVM_DBG_MIRRORPIN_WHY=1 \
    cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -Xlog:class+unload \
    -cp tools/bench $p 2>&1 | grep -E 'conc-unload |\[probe\]|conc_unload:|conc-unload-why|unloading class'
done
```
- stdout, all four runs:
  `conc-unload dead-loader-unloaded=true dead-class-unloaded=true control-loader-alive=true control-ok=true`
- CratonVM: `[probe] rounds=R old_collections_at_unload=0`;
  `[GC] conc_unload: concunload_remarks>=1 ... concunload_loaders>=1 concunload_classes>=1`;
  one `unloading class GenR5W5ConcUnloadProbe$Payload 0x...` line (resp. W3's
  Payload), and the remark's `[conc-unload-why]` line for DEAD's loader shows
  `marked=false live_or_ineligible=false`.
- **If W5 passes and W3 fails:** the retention is main-thread state; W3's
  `[conc-unload-why] pause=initial-mark` line for DEAD's loader names the root
  source (`frozen_peers` / `collect_roots` = a stale compiled or native frame
  word; `thread_snapshots` = a stale deposit or the lock stack).
- **If both fail:** the lines name it: `old=false` = DEAD never promoted
  (pinned young; then `CRATONVM_GEN_PINNED_YOUNG_COPY=1` is the control arm);
  `roots=-` with `marked=true` = reached through the heap (a mirror row or a
  field: compare the mirror's own line).
- `--nojit` arm of both, as a control for the conservative-JIT hypothesis.

## STATUS (2026-09-26, gen r5w4/conc8, superseded above): the probe now OPENS a concurrent cycle (wave 3's never did); expected output below; still opt-in, unbuilt

**Why wave 3's run showed nothing.** The orchestrator's run on `897210f83`
printed `dead-loader-unloaded=false` on every arm and
`[GC] conc_unload: concunload_remarks=0`. No concurrent cycle ran at all. The
probe's "promotion ring" held 16 MiB of blocks, and each block lived for about
25 MiB of allocation, less than one 64 MiB young semi-space (`-Xmx256m`). So
every block died young, and the 128 MiB old generation never reached the
concurrent start. That start is `ConcurrentStartPolicy::threshold`: 45 % of
capacity until a cycle has been measured, adaptive after that. HotSpot Serial,
for the same reason, ran no full collection and printed `false` too.

**The rework (`tools/bench/GenR5W3ConcUnloadProbe.java`).** Each round replaces
one 34 MiB `long[]` "ballast", then churns 80 MiB of young garbage. On
CratonVM an array larger than half a young semi-space (32 MiB) is humongous and
goes straight to the old generation. The occupancy after each round is
therefore:

| Round | Old-gen occupancy | What happens |
|---|---|---|
| 0 | base + 34 MiB | |
| 1 | base + 68 MiB, about 57 % | Past the 45 % start. Below the 75 % STW floor whenever round 0 did not already open a cycle (`base < 23.6 MiB`). |

The young collection that the churn forces asks the trigger (the
allocation-failure door, default-on). The cycle opens with DEAD already
unreachable, and its remark unloads DEAD. Other changes:

- DEAD is never touched by a method that can be compiled (`promote()` no
  longer calls it), so no stale JIT spill slot can keep it alive.
- The loop runs 4 rounds past the unload, so that a later cycle can release the
  retained layout.

On HotSpot every round leaves 34 MiB of garbage in the tenured generation. The
ballast is either allocated there directly or promoted, because it fits no
survivor space. The resulting full collection unloads DEAD within a handful of
rounds. **No env knob is needed for the verdict.** The first cycle is the one
that must unload, and it is forced by the occupancy.

**Expected (orchestrator).**
```
java -XX:+UseSerialGC -Xmx256m -cp tools/bench GenR5W3ConcUnloadProbe
CRATONVM_GEN_CONC_CLASS_UNLOAD=1 CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1 CRATONVM_DBG=gc-stats \
  cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR5W3ConcUnloadProbe
```
- **stdout, identical on both:**
  `conc-unload dead-loader-unloaded=true dead-class-unloaded=true control-loader-alive=true control-ok=true`
- **CratonVM stderr, flag arm:**
  - `[probe] rounds=R old_collections_at_unload=0`, with `R` 0, 1 or 2.
  - `[GC] conc_unload: concunload_remarks>=1 ... concunload_loaders>=1 concunload_classes>=1 concunload_layouts_retained>=1 ...`
  - `[GC] conc_driver: ... concdrv_cycles_completed>=1`
  - `[GC] conc_policy: ... concpol_old_capacity=` about `134217728` (the
    128 MiB the arithmetic assumes).
- **Default arm (no flags).** Concurrent cycles run but unload nothing. The
  later rounds cross the STW floor, and DEAD goes at a stop-the-world major:
  `true` stdout with `old_collections_at_unload>=1`, or `false` if no major ran
  within 64 rounds. Either is a PASS for this page. Only the flag arm must show
  `=0`.
- **Knob for the tail.** `CRATONVM_GC_CONC_START_PERCENT=40` keeps the tail
  rounds' cycles concurrent too. After the first cycle the adaptive start is
  about 72 %: a single-threaded inline cycle measures no growth. Use the knob
  for the layout-release check (see
  `gengc-r5w3-unload7-retained-layouts-are-never-released-20260926.md`).
- **If `concunload_remarks=0` again,** read `[GC] conc_doors:` and
  `[GC] conc_policy: concpol_old_used=...`:
  - `alloc_fail_asked>0` with `due=0` means the occupancy assumption is wrong,
    for example the old-gen capacity is not 128 MiB;
  - `asked=0` means no door asked.

## STATUS (2026-09-26, gen r5w3/unload7, superseded above for the probe): FIX LANDED opt-in (`CRATONVM_GEN_CONC_CLASS_UNLOAD`), unbuilt — all four parts of the design, plus a THIRD hazard the design missed

**What landed** (one flag, default off; takes effect only together with
`CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1` and loader unloading on, the default):

1. **Roots** (`vm/src/memory/roots.rs`). `generational_metadata_is_conditional`
   takes a fourth input, `gc_quiescence::class_unload_marking()`, which
   LICENSES the scan whatever the phase says. On Generational only the
   concurrent driver sets it, around its initial-mark and remark
   `collect_roots`, and only under the flag. Every other scan taken while a
   cycle is open (young pauses, `System.gc()`) keeps the veto. Truth-table test
   updated (`the_concurrent_old_gen_mark_vetoes_conditional_metadata`).
2. **Marker** (`gc/src/concurrent_mark.rs`). New `ClassUnloadTables`: a
   SNAPSHOT of `loader_pin` (the rows inside this old generation,
   `snapshot_where`), `mirror_pin` and `metadata_pin`, captured in the
   initial-mark pause AFTER the root scan (hazard 1: the live `metadata_pin`
   is rebuilt at every pause).
   - `scan_object_into` follows, for every scanned object, its class's
     `loader_pin` row and — when the object is a loader — its mirror and
     metadata rows. One acquire load when unarmed.
   - `mark_rows_of_live_owners`: at the initial mark it marks the rows of every
     owner that is not sweep-eligible (young loaders: hazard 2, loader half);
     at the remark, after the root rescan, the rows of every owner already
     live. The remark's own capture is merged in first
     (`add_class_unload_tables`), so the static values the REMARK deferred are
     followed for a loader that turned black before the pause. One pass is
     enough: an owner marked later is scanned, and its scan follows the merged
     tables.
   - Released at the end of `remark_finish`, and by `abort_cycle` /
     `finish_sweep`.
3. **Driver** (`vm/src/runtime/interpreter/gc_and_alloc.rs`).
   - Initial mark and remark take `collect_roots` under
     `with_class_unload_marking`, capture the tables, and add the defining
     loader of every YOUNG object's class as a root
     (`gen_conc_young_instance_loaders`; hazard 2, instance half).
   - The remark runs the reference processing WITHOUT the reconcile, then
     `remark_finish`, then the reconcile + unload on the final bitmap
     (`gen_remark_unload_classes`), still inside the pause with the old-gen
     guard released. Reconciling first (G1's order) would unload the class of
     an object the same remark then resurrects (a dead finalizable, a soft
     survivor); after `remark_finish` a kept object has been scanned, and its
     scan marked its loader.
4. **Reconcile.** `reconcile_class_mirrors` → `gc_reconcile_defining_loaders` →
   `unload_dead_class_metadata`, as G1's remark, but see 5.
5. **Hazard 3 (not in the design): the layout of an unloaded class must outlive
   its dead instances.** `ClassStore::remove` unregisters an unloaded class's
   compact field layout, and a compact object's SIZE is read from that layout
   (`object_instance_size` answers `IMPLAUSIBLE_BODY_SIZE` without it). Every
   stop-the-world caller frees the dead instances in the same pause, before
   the transaction. The concurrent sweep frees them AFTER it, and its walk
   (`OldGen::walk_objects_from`) sizes every object it steps over: the first
   dead compact instance of an unloaded class would break the walk
   (`RegionScan::Break`), leaving that region's tail unswept, and every later
   walk of the generation (the next cycle, young dirty-card scans, compaction)
   would break at the same place. Fixed by
   `memory::gc::with_retained_unloaded_layouts`: the transaction puts each
   unloaded class's current layout back (inert: the id is a tombstone nothing
   can allocate against). Both generational remark paths run under it. The
   layouts are never released — filed as
   `gengc-r5w3-unload7-retained-layouts-are-never-released-20260926.md`.

**Residual question answered.** A live mirror's loader IS reachable through a
heap field: `vm_object::class_mirror_impl` stores the defining loader into
`Class.classLoader` whenever `classloader::defining_loader_for` has a pairing
when the mirror is minted, and that same condition is what adds the
`mirror_pin` row. A mirror minted without the pairing has no row either, so
roots.rs step 6 roots it outright (`mirror_deferral_is_covered`) and it is
never deferred. A young mirror's field is a young→old root. No extra
mirror→loader edge is needed.

**Census** (`--verbose:gc` / `CRATONVM_DBG=gc-stats`, printed only under the
flag): `[GC] conc_unload: concunload_remarks=R concunload_edges=E
concunload_loaders=L concunload_classes=C concunload_layouts_retained=K`.

**Probe** `tools/bench/GenR5W3ConcUnloadProbe.java` (orchestrator runs it):

```
javac -d tools/bench tools/bench/GenR5W3ConcUnloadProbe.java
java -XX:+UseSerialGC -Xmx256m -cp tools/bench GenR5W3ConcUnloadProbe
CRATONVM_GEN_CONC_CLASS_UNLOAD=1 CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1 CRATONVM_DBG=gc-stats \
  cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR5W3ConcUnloadProbe
```

Expected stdout, identical on both:
`conc-unload dead-loader-unloaded=true dead-class-unloaded=true control-loader-alive=true control-ok=true`.
Expected CratonVM stderr: `[probe] rounds=R old_collections_at_unload=0` (HotSpot
prints `>=1`: its full collections are what unload there), and `[GC] conc_unload:`
with `concunload_remarks>=1 concunload_loaders>=1 concunload_classes>=1`.
`control-loader-alive=true` is hazard 2's control (the loader kept only by a
young instance). The same CratonVM run WITHOUT the flag must print the same
stdout only with `old_collections_at_unload>=1` (a STW major did it) and no
`[GC] conc_unload:` line.

**Unit tests (unrun):**
`cargo test -p cratonvm-gc --lib r5w3_` (four in `concurrent_mark.rs`: the
three edges, the rows of unjudgeable owners, `absorb`, the door census; one in
`reference.rs`), `cargo test -p cratonvm-vm --lib the_concurrent_old_gen_mark_vetoes_conditional_metadata`,
`cargo test -p cratonvm-vm --lib r5w3_layout_retention_is_scoped_to_the_caller`,
`cargo test -p cratonvm-types --lib the_satb_old_only_and_softref_hotspot_lru_switches_are_opt_in`,
and the tripwires `cargo test -p cratonvm-vm --test g1_class_unloading_wired`
(unchanged: G1's remark body still calls both) and
`cargo test -p cratonvm-vm --lib the_unload_transaction_forgets_every_loader_conditional_table`.

**Known residuals.**
- The per-object side-table lookup takes a `parking_lot::RwLock` read and up to
  three `FxHashMap` probes while armed (G1's `MarkSideTables` shape);
  `gengc-r5w3-unload7-proposal-side-table-lookups-per-drain-not-per-object-20260926.md`.
- A user-loader class that stays loaded under a BUILT-IN namespace after its
  loader dies (the orphan-marker case) had its statics deferred to that loader;
  the transaction does not remove them. Shared with every loader-conditional
  collector (STW major, G1); filed as
  `../../internal/gc/gengc-r5w3-unload7-orphaned-class-statics-deferred-to-a-dead-loader-FIXED-20260928.md`.

## STATUS (2026-09-26, gen r5w2/conc6, superseded above): OPEN, NARROWED. No code landed; the design below is sharpened with two hazards the wave-1 fix plan would have hit

**Why nothing landed.**

- The switch that makes anything unloadable is the veto in
  `vm/src/memory/roots.rs` (`generational_metadata_is_conditional`). No lane
  owns that file this wave.
- Without lifting the veto, the marker-side edges below only over-mark. They
  would be about 200 unbuilt lines on the concurrent marker's hottest loop, with
  nothing to measure them by.
- A wrong edge set here is not a leak, it is a use-after-free. The concurrent
  sweep frees every unmarked eligible object, and a loader it frees while one of
  its classes is still in use leaves the class tables pointing at freed memory.
  Changing the root set and the marker in two different waves, unbuilt, is the
  wrong way to land that.

**Two hazards in the wave-1 plan** (its steps 1 to 3 below):

1. **The pin tables are rebuilt at every pause, so the marker cannot read them
   live.** `collect_roots` begins with
   `metadata_pin::replace_metadata_pins(vm, &[])` (`roots.rs`, before step 1)
   and re-records only what THIS root scan defers.
   - Young pauses run between the initial mark and the remark. While a cycle is
     marking they run vetoed, so they record nothing and the table stays empty.
   - A marker that looks `metadata_pin::roots_for_loader` up while scanning a
     loader in Phase 2 therefore finds none of the rows the initial mark
     deferred. The deferred static values are unmarked and swept.
   - Statics carry an SATB pre-barrier (`jit_putstatic_*`, the interpreter's
     `putstatic`), which covers the static rows. It does not cover the other
     row sources: class-atomic slots, annotation proxies, `ClassValue`, indy
     call sites, the ObjectStreamClass cache, condy.
   - **Fix:** the cycle snapshots the three tables (`loader_pin::snapshot`,
     `mirror_pin::snapshot`, `metadata_pin::snapshot`) inside the initial-mark
     pause, AFTER `collect_roots`, and the marker follows the snapshot.
   - This is the generational analogue of G1's `MarkSideTables`, but
     snapshot-at-the-beginning, not refreshed. G1 refreshes on an epoch because
     its pauses move objects. A generational young pause moves only young
     objects, and those are never sweep candidates.
2. **The concurrent marker never scans young objects, so the
   instance → defining-loader edge from a young instance is invisible.**
   - Suppose a class's mirror is deferred (the point of unloading) and its only
     live instances are young. Then its loader is reachable only through those
     instances' `loader_pin` edge.
   - `collect_young_to_old_roots` pushes FIELD edges only.
   - **Fix:** in the initial-mark and remark pauses, walk the young objects
     (`GenerationalHeap::walk_young_objects`, the walk
     `collect_young_to_old_roots` already makes) and push:
     - `loader_pin[class_id]` of each young object;
     - the `mirror_pin` / `metadata_pin` rows of each young object that is
       itself a loader. A young object is live by assumption, so its rows are
       live.

**The sound design, all four parts or none, behind one opt-in flag:**

1. **`roots.rs` (cross-lane request).** `generational_metadata_is_conditional`
   lifts the veto when `gc_quiescence::class_unload_marking()` is set. That is
   G1's rule in the same function. The veto stays for every other root scan
   taken while a cycle is open.
2. **Driver** (`gc_and_alloc.rs`, `maybe_concurrent_gc_at` and
   `gen_concurrent_remark_pause`):
   - take the initial-mark and remark `collect_roots` under
     `with_class_unload_marking`;
   - snapshot the tables right after the initial mark's root scan;
   - add the young-object edges of hazard 2 to both pauses' roots.
3. **Marker** (`concurrent_mark.rs`):
   - `scan_object_into` follows `loader_pin[class_id]` for every scanned object;
   - for an object that is a loader key, it follows the snapshot's mirror and
     metadata rows. This is one `OnceLock` load per object when the flag is off.
   - The remark adds a fixpoint over the rows recorded at REMARK: every row
     whose key reads live under `remark_is_marked` (marked, or not
     sweep-eligible) marks its values, then drains, repeating until nothing new
     is marked. That covers loaders black before the remark recorded their
     rows.
4. **Reconcile.** Already wired: with `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK`
   the remark runs `reconcile_class_mirrors` → `gc_reconcile_defining_loaders` →
   `unload_dead_class_metadata` (through `g1_remark_process_references`), with
   the old-gen guard released. Nothing is needed "after the sweep": the verdict
   is the remark's bitmap, and the sweep frees what the reconcile dropped.

**Residual question before any of it lands.** Is a live `java.lang.Class`
mirror's loader always reachable through a heap FIELD? That is
`Class.classLoader`, which the marker traces as an ordinary slot. If it is
sometimes served only from `defining_loader_store`, a live mirror would not keep
its loader marked. That needs an edge too: `mirror → loader_pin[mirror's class]`.
Check `vm_object::get_or_create_class_mirror` before step 3.

**How to verify (unchanged, made concrete):**

- A generational twin of `RClassUnloadSweep`, run with
  `CRATONVM_GEN_CONC_SERVICE_THREAD=1 CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1`
  and the new flag:
  - load a class through a throwaway loader and drop every reference;
  - drive old-gen growth with no `System.gc()`;
  - the loader must be gone, with `concdrv_cycles_completed` moved and
    `major=` not.
- The same run with instances of the class held ONLY in young objects: the
  loader must NOT be unloaded. This is hazard 2's control.

---

- **Status (r5w1):** OPEN.
- **Severity:** latency. A dead class loader waits for a STW full collection, as
  before. No correctness claim.
- **Code:** `vm/src/memory/roots.rs` (the concurrent mark's veto of every
  side-table deferral, `the_concurrent_old_gen_mark_vetoes_conditional_metadata`);
  `gc/src/concurrent_mark.rs::scan_object_into` (follows no metadata-pin edge).

## What is wrong

With `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK` the remark now runs G1's reconcile
chain against the cycle's bitmap: `reconcile_class_mirrors`,
`gc_reconcile_defining_loaders` and `unload_dead_class_metadata`, inside
`g1_remark_process_references`, which the generational twin reuses. Nothing
becomes unloadable, though. The root set a concurrent generational cycle consumes
roots outright:

- every user-loader class MIRROR;
- statics;
- class locks;
- condy values and `ClassValue` results.

`ConcurrentMarker` follows no side table (`metadata_pin`, `loader_pin`), so
`collect_roots` must veto every deferral for it. A mirror names its loader. So
every loader with a live class entry is marked, and the reconcile finds none dead.

G1 unloads at remark because its marker follows the pin edges, and its initial
mark and remark take their roots under `with_class_unload_marking`, which leaves
those side tables to the marker.

## Proposed fix (M)

1. Teach `ConcurrentMarker::scan_object_into` the two pin edges G1's marker
   follows. When it scans a class-loader object, push its
   `metadata_pin::roots_for_loader` and `loader_pin` rows, and scan mirror → loader
   as an ordinary field.
2. Take the concurrent cycle's initial-mark and remark roots under
   `gc_quiescence::with_class_unload_marking`, as G1's do, so the veto lifts only
   for a marker that follows the pins.
3. SATB covers the side tables only if their writers log. A pin written during
   Phase 2 must be logged (or re-scanned at remark) like any reference store.
   Check that before step 2.

## How to verify

- A generational twin of `RClassUnloadSweep`:
  1. load a class through a throwaway loader;
  2. drop every reference;
  3. drive old-gen growth with no `System.gc()` and the service thread on;
  4. assert that the loader is unloaded by a CONCURRENT cycle
     (`concdrv_cycles_completed` moved, `major=` did not).
- A source-presence tripwire like `vm/tests/g1_class_unloading_wired.rs`.
