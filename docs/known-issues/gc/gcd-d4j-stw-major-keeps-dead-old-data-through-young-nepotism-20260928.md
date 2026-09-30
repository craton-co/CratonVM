# A Generational STW major keeps dead old data alive through young objects a dead old object names

> **STATUS (2026-09-29, gce e2/c, later): step 1 `CRATONVM_GC_FULL_GC_TRUE_ROOTS_FREE_YOUNG` REMOVED (rejected: it does not do its job -- `fy_nsoom` 0/5 like the off arm -- and it breaks `NativeGrowthReclaimProbe` with OSR off, `fy_ngr_noosr`).** Removed from `gc/src/gen_heap.rs` (the reader, `TrueRootAsk::free_young`, `TrueRootMarkOut::reclaim`, `TrueRootSweepOut::young_freed`, `TrueRootYoung::reclaimable_runs` and its `unplain` list, the step-1 block of `sweep_old_gen_non_moving_impl`, the test `gcd_d9a_step1_reclaimable_runs_...`; `gcd_d9a_step1_reclaims_...` became `gcd_d9a_the_excluded_young_survivor_waits_for_the_next_young_collection`) and `gc/src/old_gen.rs` (`note_true_root_young_freed`; the `oldsz_true_root_young_freed_bytes` key stays, always 0). The registry rows (`types/src/flag_groups.rs`, `types/tests/flag-surface.txt`) go with lane o's removal list. The page stays KEEP for the one-major ladder; the census row below is next.

> **STATUS (2026-09-29, gce e2/c): KEEP -- reading (b) REFUTED by the e1 rows; step 1 (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_FREE_YOUNG`) recommended for REMOVAL.** `fy_nsoom_1..5` (step 1 on) and `fy_nsoom_off_1..5` both FAIL 0/5 with `CRATONVM_GC_OVERHEAD_PROGRESS=0` (base binary, `verify-e1/flip-base`), so freeing the excluded young survivors in the pause does not let one major free the dropped data, and step 1 regresses `fy_ngr_noosr` (details and the removal list: `docs/internal/gc-design-perf-round-20260929/e2-c-report.md` section 2). The second-major workaround (`CRATONVM_GC_OVERHEAD_PROGRESS`, default on) keeps the default arm =HS (`opc_nsoom` SAME). Next, the decisive census row (keep the .err): `p d4j_census 300 "CRATONVM_GC_OVERHEAD_PROGRESS=0 CRATONVM_DBG=gc-stats,oldmark-root-census,root-source RUST_LOG=cratonvm::gc=info" "-XX:+UseGenerationalGC -Xmx64m" GenR4W4NativeStringOomProbe` -- the last census block before the escaping OOME names the holder.

> **STATUS (2026-09-29, gce e1/x): KEEP -- neither retire arm passes.**
> - Evidence (`docs/internal/gc-design-perf-round-20260929/verify-e1/flip-base`, base binary `adb9178bc`, `GenR4W4NativeStringOomProbe -Xmx64m`, `CRATONVM_GC_OVERHEAD_PROGRESS=0`): step-1 arm (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_FREE_YOUNG=1`) `fy_nsoom_1..5` 0/5; step-1-off control `fy_nsoom_off_1..5` 0/5. The same HotSpot cache entry passed `opc_nsoom_1..2`, so the failures are real. The default arm (second major on) is 19/20 on base and e1 (orchestrator). With FREE_YOUNG on, `fy_ngr_noosr` fails (`NativeGrowthReclaimProbe`, OSR off), so the switch was not flipped.
> - **Remaining:** re-run both arms on a build with e1/c's in-mark closure (e1 or later) with `CRATONVM_DBG=gc-stats`, and read `oldsz_true_root_*`; the one-major arm fails with step 1 off too, so reading (a) is not the whole story. Explain `fy_ngr_noosr` before any flip.

> **STATUS (2026-09-29, gce e1/c): OPEN (awaiting the d9/a probe run below), reviewed -- no defect in the landed d9/a code.** Re-read `TrueRootYoung::reclaimable_runs` and the step-1 consumer in `sweep_old_gen_non_moving_impl` (zeroed runs inside `young_from.used()`, recorded in the span ring, published to the young free list only when the sweep freed exactly the unmarked set): sound. gce e1/c's in-mark live-set closure (`gcd-d9a-true-root-seed-skips-young-targets-of-close-live-set-rescues-20260928`) keeps step 1 refused on any rescue (`pre_rescued == 0` joins `rescued == 0`), and the true-root major now takes one `watched_referents_snapshot` clone instead of two. The d9/a run block below is unchanged and still decides.

> **STATUS (2026-09-28, gcd d9/a): both remaining halves LANDED -- the promoting-cycle fallback (default on) and the young half (opt-in); awaiting the probe.**
>
> - **Where d8/x left it (`307f0c6a2`):** the one-major arm (`CRATONVM_GC_OVERHEAD_PROGRESS=0 ... GenR4W4NativeStringOomProbe -Xmx64m`) printed only `fill: OutOfMemoryError "Java heap space"` 5/5, `nsoom_trueRootsOff` the same, the default arm passed 5/5 on the ladder's second major. The two readings of d5/r were both open: (a) the requested major's young phase PROMOTED, so the seed fell back (every destination a root); (b) the seed ran, but the young survivors it left out stayed allocated in young until the next young collection.
> - **Landed (gcd d9/a, `gc/src/gen_heap.rs`):** (a) the true-root seed on a promoting cycle and under a planned pinned compaction (`TrueRootAsk`, `TrueRootYoung::promoted_index` / `round`, dead promotions dropped from the pointer map; default on, `CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE=0` restores d8); (b) step 1 of the d5/r proposal, option (B): `TrueRootYoung::reclaimable_runs` + `sweep_old_gen_non_moving_impl` zero the excluded young survivors onto the young free list in the same pause, only when the in-place sweep freed exactly the unmarked set (no walk gap, no `close_live_set` rescue, no pinned compaction). Opt-in: `CRATONVM_GC_FULL_GC_TRUE_ROOTS_FREE_YOUNG=1`. The legacy seed also now follows the `mirror_pin` / `metadata_pin` rows of every young owner and of every owner promoted in the pause (a use-after-free candidate by reading; the moving half is `gcd-d9a-moving-major-misses-side-table-rows-of-moved-owners-FIXED-20260929.md`).
> - **Unit tests:** `cargo test -j 5 -p cratonvm-gc --lib gcd_d9a_` -> 8 passed, among them `gcd_d9a_a_promoting_requested_major_keeps_the_true_root_seed_and_drops_a_dead_promotion` (this page's shape with `y` promoted) and `gcd_d9a_step1_reclaims_the_excluded_young_survivors_only_when_asked`.
> - **Run (orchestrator, Linux release, JIT on, 5 runs each arm):**
>   ```
>   P="--java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench"
>   for i in 1 2 3 4 5; do CRATONVM_GC_OVERHEAD_PROGRESS=0 CRATONVM_GC_FULL_GC_TRUE_ROOTS_FREE_YOUNG=1 CRATONVM_DBG=gc-stats timeout 300 cratonvm $P GenR4W4NativeStringOomProbe 2>ns.$i.err; echo rc=$?; grep -h 'oldsz_true_root' ns.$i.err | tr ' ' '\n' | grep true_root; done
>   for i in 1 2 3 4 5; do CRATONVM_GC_OVERHEAD_PROGRESS=0 timeout 300 cratonvm $P GenR4W4NativeStringOomProbe; echo "[step1 off] rc=$?"; done
>   for i in 1 2 3 4 5; do timeout 300 cratonvm $P GenR4W4NativeStringOomProbe; echo "[default] rc=$?"; done
>   ```
>   Expected: the probe's own four lines (it has no HotSpot oracle: `../../internal/gc/gcd-d8x-native-string-oom-probe-is-not-a-hotspot-oracle-FIXED-20260929.md`) `fill: OutOfMemoryError "Java heap space"`, `native-strings ok`, `recovered ok`, `PASS` -- 5/5 on the step-1 arm with `oldsz_true_root_fallbacks=0` and (once lane b prints it) `oldsz_true_root_young_freed_bytes>0`; the default arm stays 5/5. The step-1-off arm is the control: if it now passes too, reading (a) was the whole story and step 1 needs no flip.
> - **Retire when** the step-1 arm passes 5/5 and step 1 is flipped on (its gate: `gcd-d9a-proposal-true-root-seed-for-every-stw-major-20260928.md` section 3), or the step-1-off arm passes 5/5 by itself.

> **STATUS (2026-09-28, gcd d5/r): OLD half FIXED for requested majors
> (d4/n, default ON, `CRATONVM_GC_FULL_GC_TRUE_ROOTS=0` keeps the legacy
> seed); a use-after-free in it FIXED (d5/r, unbuilt); the YOUNG half is
> still open, and lane j's second major (`CRATONVM_GC_OVERHEAD_PROGRESS`,
> default on) is what closes it today -- keep it.**
>
> * **What d4/n landed** (`gc/src/gen_heap.rs`, `TrueRootYoung` /
>   `TrueRootMajorScope`; `run_non_moving_young_cycle` arms it,
>   `sweep_old_gen_non_moving_impl` hands it to `old_gen_gc_inner`): a
>   REQUESTED major's non-moving old sweep seeds young from the true roots
>   (the root slice resolved against a from-space walk, every finalizable
>   candidate, unparseable stretches as roots, and to a fixed point the young
>   targets of MARKED old objects) instead of from every young survivor.
>   Legacy fallbacks: the cycle promoted anything, the old walk has a gap, a
>   pinned compaction is planned.
> * **Measured (orchestrator, d916d1c40, Linux):** `GenR4W4NativeStringOomProbe
>   -Xmx64m` passes 3/3 on the default build, but FAILS 3/3 with
>   `CRATONVM_GC_OVERHEAD_PROGRESS=0` (and 2/2 with `TRUE_ROOTS=0` too). So
>   the true-root seed alone does not let ONE major free the dropped data.
> * **Why, by reading (d5/r).** Two candidates; the run below decides.
>   (b) is the likelier:
>   (a) *a fallback fires*: the ladder's major runs on a young sweep that
>   promotes (near OOM the promote-on-pressure arm tenures survivors into
>   whatever old holes remain), and every promotion destination is a root of
>   the legacy seed, so the dead `elementData` promoted in that pause keeps
>   the whole chain;
>   (b) *no fallback, the young half*: the true-root major frees the dropped
>   OLD data, but the young survivors it left out (`oldsz_true_root_young_excluded`:
>   the young objects the young phase kept only for a dead old holder --
>   here the dropped lists' young `elementData` and their young content) stay
>   ALLOCATED in young until the NEXT young collection. The failing
>   allocation is a young one, so without a second collection young is still
>   full and the ladder throws. HotSpot's full collection frees young garbage
>   in the same pause; this collector's true-root major proves the young
>   objects dead but does not free them. Lane j's second major is exactly
>   the missing young collection.
> * **The run that decides** (Linux, release, JIT on, 3 runs):
>   ```
>   CRATONVM_GC_OVERHEAD_PROGRESS=0 CRATONVM_DBG=gc-stats CRATONVM_DBG_PROMO_SEED=1 RUST_LOG=cratonvm::gc=info \
>     cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx64m -cp tools/bench GenR4W4NativeStringOomProbe 2>&1 \
>     | grep -E 'legacy young seed|promo-seed|oldgen_sizing|fill:|native-strings|recovered|PASS|FAIL'
>   ```
>   (a) shows `requested major kept the legacy young seed ... why="the young
>   phase of this pause promoted objects ..."` lines (d5/r's new rate-limited
>   `info`, `note_true_root_fallback`) and `[promo-seed] old sweep seeded N`
>   with N>0 on the ladder's majors, and `oldsz_true_root_fallbacks` about
>   equal to the ladder's majors; (b) shows `oldsz_true_root_majors>=1`,
>   `oldsz_true_root_young_excluded` in the thousands and no fallback line.
>   On the d916d1c40 binary (no `info` line yet) `CRATONVM_DBG_PROMO_SEED=1`
>   alone separates them.
> * **What d5/r fixed (review of the unbuilt d4/n code; default path).** The
>   true-root seed left out a young survivor that a WEAK table hands back
>   after the pause: the young phase keeps it (a dead old holder's card names
>   it), the post-collection restore then returns it through its
>   `WeakReference` / `SoftReference` / JNI weak global as a survivor
>   (`VmHeap::watched_pre_gc_addr_survived`), and an enqueue writes into its
>   queue -- while the old sweep had freed its old referents and the queue:
>   a use-after-free. `old_gen_gc_inner` now also seeds the young trace from
>   the watched set the VM published for the collection
>   (`gc_quiescence::watched_referents_snapshot`: every address the
>   reference processor holds, plus every JNI weak global's referent) --
>   over-retention for those objects only, exactly as the legacy seed. Also:
>   a planned pinned compaction's declined ask is now counted as a fallback
>   (it was counted nowhere), and every fallback says why on a rate-limited
>   `info` line (target `cratonvm::gc`).
>   Test: `cargo test -j 5 -p cratonvm-gc --lib gcd_d5r_a_watched_young_survivor_keeps_its_old_referent_through_a_true_root_major`
>   -> 1 passed; `cargo test -j 5 -p cratonvm-gc --lib gcd_d4n_` unchanged.
> * **Still open (young half; owner: the young-copy lane with the old-gen
>   lane):** free the true-root major's excluded young survivors in the same
>   pause, or run the second young sweep inside the collector call. Design and
>   hazards: `gcd-d5r-proposal-true-root-seed-beyond-requested-non-promoting-majors-20260928.md`.
>   Until then do NOT remove `CRATONVM_GC_OVERHEAD_PROGRESS`'s second major.
> * **Retire when:** the young half lands and `CRATONVM_GC_OVERHEAD_PROGRESS=0
>   ... GenR4W4NativeStringOomProbe -Xmx64m` prints HotSpot's four lines
>   (`fill: OutOfMemoryError "Java heap space"`, `native-strings ok`,
>   `recovered ok`, `PASS`) 5/5.

*Filed 2026-09-28 by gcd d4/j (lane oom ladder 2), from reading while
working the heap-full probe regressions. Unmeasured: the runs below decide
it. The allocation ladders now work around it (two majors before an OOME);
the collector half is this page.*

- **Status:** OPEN.
- **Severity:** wrong result: a spurious `OutOfMemoryError` right after a
  program drops its data (HotSpot's Serial full collection frees it), and
  floating garbage that survives one extra major in general.
- **Owner:** lane n (the old-gen / Phase 5 parts of `gc/src/gen_heap.rs`:
  `mark_young_to_old_refs` and the STW major that calls it).

## What is wrong, by reading

1. The young phase of a pause treats the OLD generation as live (minor
   collection semantics): every young object an old object names through a
   dirty card survives, whether that old object is live or dead.
2. The Phase-5 STW major then seeds the old-generation mark from every young
   object that survived the young phase (`GenerationalHeap::mark_young_to_old_refs`;
   the comment beside it: "young from-space is conservatively retained for this
   major cycle").

So a DEAD old object that names a young object keeps, through that young
object, everything the young object reaches in the old generation alive
through the major. The dead old object itself is swept; the next pause's young
phase then frees the young object, and a SECOND major frees the rest. A chain
that alternates k times between the generations needs k majors.

Shapes the OOME probes build every run:

- a dropped `ArrayList` that has lived long enough to be old, whose last
  backing array (`elementData`, grown by `Arrays.copyOf`) is still young:
  `GenR4W4NativeStringOomProbe`'s `made` and `fill`;
- a dead linked chain whose NEWER nodes were tenured in place (pinned
  in-place promotion under the JIT) ahead of OLDER nodes still young:
  `GenR4W4HeapFullThrashProbe`'s `chain` / `threads`,
  `GenR4W5ThreadsOomProbe`. The more in-place tenuring the young path does
  (gen r5, gcd d2/h, d3/m), the more alternation, which fits the regression
  of those probes on the d2/d3 builds.

The allocation ladders used to decide the OOME after ONE major (the soft
rung's or the overhead-limit one), so the first allocation after the drop
threw from `CharBuffer.wrap` (`println`) with the heap reclaimable.

## Workaround landed (gcd d4/j)

`vm/src/runtime/interpreter/gc_and_alloc.rs`: `majors_to_decide_oome` (a
second major when the first freed under 2 % of the heap) in the
overhead-limit arm of `collect_and_retry_with_thread`, `second_major_before_oome`
after the last-ditch rung, and the native funnel's owed collection. The JIT
helpers need the same (cross-lane request to lane o in
`docs/internal/gc-defects-round-20260927/d4-j-report.md`).

## Proposed fix (collector)

For a major the ladder REQUESTS (the thread-local request, `major_gc_requested`)
at least, mark young from the roots together with old (HotSpot's full-GC
semantics): seed the old mark only from young objects reached from the roots
or from MARKED old objects, iterating to a fixed point (a young object reached
from a newly marked old object is marked and its old referents pushed). The
gen r5w6/conc10 live-young seed (`CRATONVM_GEN_Y2O_LIVE_SEED`) is not it: it
treats every old object as live, which is exactly the edge that keeps the dead
data. Keep today's seed as the fallback when the young grid cannot be walked.

## How to verify

```
P="--java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench"
CRATONVM_DBG=oldmark-root-census,root-source timeout 300 cratonvm $P GenR4W4NativeStringOomProbe 2>ns.err
grep -n 'cat=young/' ns.err | tail -5
```

Confirmed when the last major before the escaping OOME (run with
`CRATONVM_GC_OVERHEAD_PROGRESS=0`, which restores the one-major ladder)
attributes the dropped data to `cat=young/from-space-unrooted` (or the young
seed row). Fixed when, with the ladders' second majors disabled
(`CRATONVM_GC_OVERHEAD_PROGRESS=0`), the probe prints HotSpot's four lines
(`fill: OutOfMemoryError "Java heap space"`, `native-strings ok`,
`recovered ok`, `PASS`) 5 of 5.
