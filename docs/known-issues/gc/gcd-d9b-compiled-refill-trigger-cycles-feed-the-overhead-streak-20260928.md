# The compiled refill trigger's ordinary young cycles are judged by the GC-overhead limit; the interpreter's are not

> **STATUS (2026-09-29, gce ve2): OPEN -- keep CRATONVM_GC_REFILL_TRIGGER_UNJUDGED off: 0 unjudged=refill-trigger lines on every Generational ru1 row, the arm never engaged.** Evidence: `docs/internal/gc-design-perf-round-20260929/ve2-verdicts.md`; next steps: `gce-handoff-gc-design-perf-round-close-20260929.md`.

> **STATUS (2026-09-29, gce e2/o): OPEN (opt-in) -- the A/B must run on a binary WITH gce e1/o's reset-only arm; the e1 run was on the base.** The e1 `ru` rows ran the base binary (Gen 3/3 both arms; ZGC `ru0` 2/3 vs `ru1` 1/3; G1 every row `rc=124` in both arms, the G1 page's). On the base, arm 1 skipped `note_gc_productivity` entirely, so a latched streak could not be reset by the trigger's productive cycles -- exactly the difference e1/o's reset-only arm closes, and a candidate for ZGC's 1/3. **Rows (e2 binary; G1 excluded -- both arms time out there, `gcd-d10v-g1-...`):**
> ```
> RU="CRATONVM_GC_REFILL_TRIGGER_UNJUDGED=1"
> for gc in gen zgc; do for r in 1 2 3 4 5; do
> p ${gc}_ru0_threadsoom_$r 300 "CRATONVM_GC_STATS=1 CRATONVM_DBG_GC_OVERHEAD=1"     "$X -Xmx128m" GenR4W5ThreadsOomProbe
> p ${gc}_ru1_threadsoom_$r 300 "CRATONVM_GC_STATS=1 CRATONVM_DBG_GC_OVERHEAD=1 $RU" "$X -Xmx128m" GenR4W5ThreadsOomProbe
> done
> p ${gc}_ru1_jor 300 "$RU" "$X -Xmx64m" GenR4W6JitOomRootProbe
> p ${gc}_ru1_thrash 300 "$RU" "$X -Xmx128m" GenR4W4HeapFullThrashProbe
> p ${gc}_ru1_pcallee 300 "$RU" "$X -Xmx64m" Gcd1PinnedCalleeOomeProbe
> done
> ```
> **Decisive, per collector:** stdout = HS (the seven lines) in `ru1` at least as often as in `ru0` over the 5 pairs, and no `rc=124` in `ru1` that `ru0` does not have. On Generational additionally, from each `ru1` stderr: `[GC] oome_ladder:` `ladder_forced_unproductive <= ladder_forced`, and `grep -c 'unjudged=refill-trigger' > 0` (the arm engaged; `reset=true` lines are the resets the base arm lost). The companion rows (`ru1_jor`, `ru1_thrash`, `ru1_pcallee`) = their `ru0`/default pass rates. Flip when both collectors meet it; the flip hunk is in the e1/o block below.

> **STATUS (2026-09-29, gce e1/x): KEEP -- the flip is UNDECIDED; its A/B ran only on the base binary, before e1/o's blocker fix.** `flip-base` (base `adb9178bc`): Generational `gen_ru0_threadsoom_1..3` and `gen_ru1_threadsoom_1..3` = HotSpot 3/3 each; `gen_ru1_jor` and `gen_ru1_thrash` = HotSpot; `gen_ru1_pcallee` DIFF (the pinned-callee probe's flake). ZGC ThreadsOom: control 2/3, switch on 1/3. G1: all six ThreadsOom rows time out on both arms. **Remaining:** the page's A/B (3 collectors x 2 arms x 3) on a binary with e1/o's fix (the unjudged arm now resets a latched streak), the arm-1 rows (`GenR4W4HeapFullThrashProbe` 12/12, `vm/tests/jit_alloc_oome_clears_soft_refs.rs`), and an explanation of ZGC's 1/3 vs 2/3.

> **STATUS (2026-09-29, gce e1/o): OPEN (opt-in, default unchanged) -- reviewed for the default flip; one blocker found and FIXED IN CODE in the opt-in arm.**
> - **Blocker (fixed):** with `CRATONVM_GC_REFILL_TRIGGER_UNJUDGED=1` the trigger's cycles skipped `note_gc_productivity` entirely, so they could no longer RESET a latched streak either -- and the streak is reset nowhere else. The interpreter's doors read the streak only after their own forced collection has been judged, but the JIT's object door (`jit_new_object_body`, `vm/src/jit/helpers.rs`) reads `gc_overhead_limit_exceeded` on EVERY entry, before any collection, and answers a latched streak with the soft-reference rung (clears every `SoftReference`) plus a major. So a compiled program that caught an OOME, dropped its data and allocated on through productive trigger cycles would keep a stale streak and pay that at its next helper entry; judged (today's default), the first productive trigger cycle resets it. Now (`vm/src/runtime/interpreter/gc_and_alloc.rs`, `maybe_gc_forced_collected` -> new `note_refill_trigger_cycle_reset_only` + pure `refill_trigger_cycle_resets_streak`) an unjudged trigger cycle clears a latched streak when `note_gc_productivity` would call it productive on its freed-bytes and free-space halves, and never raises it; it does not re-arm the progress window and moves no `[GC] oome_ladder:` counter, so the page's gate `ladder_forced_unproductive <= ladder_forced` still holds in arm 1. With `CRATONVM_DBG_GC_OVERHEAD=1` it prints `[GC_OVERHEAD] unjudged=refill-trigger ... reset=<bool>` (no `streak=` key, so the G1/ZGC `streak=` reading below is unchanged). Arm 0 (the default) is byte-for-byte unchanged.
> - **Tests:** `cargo test -j 5 -p cratonvm-vm --lib gce_e1o_refill_trigger_tests` (2 tests).
> - **Flip hunk (orchestrator, only after the A/B below passes):** `types/src/flag_groups.rs` entry `refill-trigger-unjudged`: `off_word: Some("0")`; `gc_and_alloc.rs::refill_trigger_cycles_unjudged`: `runtime_flag_on(..)` -> `cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GC_REFILL_TRIGGER_UNJUDGED")`; regenerate the flag docs. Better still, a typed `GcFlags` field (the reader runs once per forced collection, so the lookup cost is not the reason).
> - **A/B:** unchanged (below), plus one arm-1 recovery check: `GenR4W4HeapFullThrashProbe -Xmx128m` must still print its nine lines 12/12, and `vm/tests/jit_alloc_oome_clears_soft_refs.rs` must still pass (`cleared=true`).

> **STATUS (2026-09-28, gcd d10/o): fix LANDED OPT-IN (default OFF), awaiting
> the A/B; default behaviour unchanged.** The fix is in the shared overhead
> accounting, not Generational policy: every backend runs the refill trigger
> (`VmHeap::needs_gc_for_jit_allocation` answers for Generational, G1 and
> ZGC) and every one of its cycles went through `note_gc_productivity`.
>
> - **Landed:** `vm/src/runtime/interpreter/gc_and_alloc.rs`,
>   `maybe_gc_forced_collected`: with `CRATONVM_GC_REFILL_TRIGGER_UNJUDGED=1`
>   a collection won at the `tlab-alloc-shaped` site skips
>   `note_gc_productivity` (no streak change, no progress-window sample, no
>   `ladder_forced_unproductive` count), as `maybe_gc`'s occupancy cycles
>   never judge; the pause, the TLAB retire, the SATB flush, the census
>   split and the concurrent-start ask are unchanged. Read by
>   `refill_trigger_cycles_unjudged` (`runtime_flag_on`); declared in
>   `types/src/flag_groups.rs` (`refill-trigger-unjudged`, GC group) and
>   `types/tests/flag-surface.txt` (the orchestrator regenerates the flag
>   docs).
> - **Still open:** the default. Flip it only on the A/B below.
> - **A/B (orchestrator; each of `-XX:+UseGenerationalGC`, `-XX:+UseG1GC`,
>   `-XX:+UseZGC`, 3 runs per arm):**
>   ```
>   for gc in UseGenerationalGC UseG1GC UseZGC; do for arm in 0 1; do for i in 1 2 3; do
>     CRATONVM_GC_REFILL_TRIGGER_UNJUDGED=$arm CRATONVM_DBG_GC_OVERHEAD=1 timeout 300 \
>       cratonvm -XX:+$gc -Xmx128m --verbose:gc -cp tools/bench GenR4W5ThreadsOomProbe 2>t_${gc}_${arm}_$i.err
>     echo "$gc arm=$arm rc=$?"
>   done; done; done
>   grep -h '\[GC\] oome_ladder:' t_UseGenerationalGC_*.err
>   ```
>   Expected: arm 1 prints HotSpot's seven lines on every row and is not
>   slower than arm 0; on Generational `ladder_forced_unproductive <=
>   ladder_forced` in every arm-1 census (the refill trigger's cycles no
>   longer counted); the OOME-retention gates (`GenR4W6JitOomRootProbe
>   -Xmx64m`, `GenR4W4HeapFullThrashProbe`, `Gcd1PinnedCalleeOomeProbe`) at
>   their d9 pass rates in both arms. (The `[GC] oome_ladder:` census is
>   Generational-only; on G1 and ZGC read the `[GC_OVERHEAD]` lines' `streak=`
>   instead -- see `gcd-d10o-proposal-oome-ladder-census-on-every-collector-20260928.md`.)

*Filed 2026-09-28 by gcd d9/b (lane ladder9), by reading. Not reproduced;
no code change beyond a corrected comment and the census split.*

- **Status:** OPEN.
- **Severity:** low -- an inconsistency on the OOME path: on a wedged old
  generation a compiled program can reach the overhead limit's
  `OutOfMemoryError` on fewer allocation failures than the same program
  interpreted. No crash; the verdict still needs a wedged old generation and
  a full collection that leaves the streak latched.
- **Owner:** lane b (`vm/src/runtime/interpreter/gc_and_alloc.rs`).

## What is wrong

`tlab_alloc_shaped_inner` (the JIT's guarded refill, `refill_needs_young_room`)
consults the OCCUPANCY trigger (`needs_gc_for_jit_allocation`) once per few
MiB of refills and, when it fires, collects through the FORCED door:
`maybe_gc_forced_at(shared, thread, "tlab-alloc-shaped")`. The forced door
(`maybe_gc_forced_collected`) runs `note_gc_productivity` on every collection
it wins, so these ordinary young cycles are judged and can grow
`gc_unproductive_streak`. The interpreter's occupancy collections take
`maybe_gc` (`GcDoor::AllocationThreshold`), which never judges. The doc of
`note_gc_productivity` said the opposite ("Forced GCs only happen on genuine
allocation failure ... so this never fires during ordinary young-GC churn");
gcd d9/b corrected the comment and split the census
(`ladder_threshold_wedged` now counts the compiled trigger's cycles on a
wedged old generation, `ladder_forced` does not).

Consequence, by reading: with the old generation wedged, a compiled
allocation loop's refill-trigger cycles each free what was allocated since
(often under 2 % of the heap), so they are `unproductive`; eight of them latch
the streak, and the next allocation failure's ladder
(`collect_and_retry_with_thread`, `jit_g1_last_ditch_full_cycle`) takes the
overhead exit after one major instead of the forced tail. The interpreted
loop gets there only through forced (allocation-failure) cycles.

## Proposed fix

Give the refill trigger the threshold door: a `maybe_gc_forced_collected`
variant (or a `GcDoor` parameter) that runs the same pause as
`GcDoor::AllocationThreshold` and skips `note_gc_productivity`, used only by
the `tlab-alloc-shaped` site. The retire / SATB flush / concurrent-start ask
stay as they are. Changes JIT OOME timing on a wedged old generation, so it
wants the A/B below before it lands default-on.

## How to verify

```
P="--java-home $JDK -XX:+UseGenerationalGC -Xmx128m -cp tools/bench"
for i in 1 2 3; do
  CRATONVM_DBG_GC_OVERHEAD=1 timeout 300 cratonvm $P --verbose:gc GenR4W5ThreadsOomProbe 2>t_$i.err
done
grep -h '\[GC\] oome_ladder:' t_*.err
```

The census tells the two apart: `ladder_forced` excludes the refill
trigger's cycles, `ladder_forced_unproductive` counts every judged cycle,
theirs included. Before the fix, `ladder_forced_unproductive` above
`ladder_forced` (or close to it while `ladder_threshold_wedged` is large) is
this page's shape. After it: `ladder_forced_unproductive <= ladder_forced`
always, HotSpot's seven lines, and the OOME-retention gates
(`GenR4W6JitOomRootProbe -Xmx64m`, `GenR4W4HeapFullThrashProbe`,
`Gcd1PinnedCalleeOomeProbe`) at their d9 pass rates.
