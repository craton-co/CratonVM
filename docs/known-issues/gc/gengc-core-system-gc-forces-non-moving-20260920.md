# Generational: every `System.gc()` forces the non-moving young sweep

> **STATUS (2026-09-29, gce e2/c): KEEP -- P-C NARROWED to a design; not built.** The `sysgc_mv_*` misses (`dead-loader-unloaded=false`) are P-A as expected: a moving cycle's root gatherer roots every mirror. They are not a regression of gce e1/c's moving-major fix, whose control lines pass 9/9. P-C is NOT "seed from to-space": the Cheney copy keeps nepotism survivors, because the young phase treats every old object as live. A safe P-C arms the true-root seed in Phase 5 with no promotions, and it must (1) remap the watched set through `pointer_map` before seeding it, (2) seed the young resurrected finalizables and remap the pass's candidates, and (3) remap `TrueRootYoung::build`'s `loader_pin` snapshot through `pointer_map`. The moved-owner `mirror_pin` / `metadata_pin` values already root correctly (e1/c). Build P-C together with P-A. Full text: `docs/internal/gc-design-perf-round-20260929/e2-c-report.md` section 3d.

> **STATUS (2026-09-29, gce e1/x): KEEP -- still true by default; one more data point for P-A.** With the opt-in `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG=1`, `GenR5W5ConcUnloadProbe` / `GenR5W3ConcUnloadProbe` print `dead-loader-unloaded=false dead-class-unloaded=false` in 8 of 9 e1 runs and 9 of 9 base runs (`verify-e1/ve1`, `sysgc_mv_*`; HotSpot `true`): the moving arm's major does not unload a dead loader, while the side-table defect of `gcd-d9a-moving-major-...` is fixed (control lines 9/9). `syscg_loop` = HotSpot on every e1 battery. **Remaining:** P-A, P-B and P-C; add the unload result above to P-A's gate.

> **STATUS (2026-09-29, gce e1/y): STILL TRUE by default; NARROWED -- the code each precondition needs, by file and function. None landed: P-A is a class-unloading semantics change across three lanes' files, P-C is lane c's Phase 5, and P-B depends on P-C's shape; the flip is the orchestrator's once all three hold.**
>
> - **P-A (a moving `System.gc()` must follow the side tables instead of rooting every mirror and loader's metadata).** Today `young_marker_follows_side_tables()` (`gc/src/gc_quiescence.rs`) answers `false` on such a cycle, so the VM's root gatherer (`roots::conditional_loader_metadata`, vm side) roots every row unconditionally and nothing can unload. Needed:
>   1. *Young (this lane):* the Cheney closure follows `mirror_pin` / `metadata_pin` rows from every object it COPIES or PROMOTES, keyed by the object's PRE-copy address (the registries are remapped only after the collector returns): in `forward_object_impl`'s scan of a copied object (serial) and `ParEvac::scan_object` (parallel), one lookup in a snapshot taken once per cycle (`cratonvm_types::mirror_pin::snapshot()`, `metadata_pin::snapshot()`; `None` when the registry is empty, so the common cycle pays one branch), forwarding each value as a root. Snapshot lookups are read-only and `Sync`, so the parallel arm can share one.
>   2. *Old owners:* a young cycle treats old gen as live, so every row whose OWNER is old is a root of the young closure: seed them once per cycle from the snapshot (filter `OldGen::contains(owner)`).
>   3. *Pinned identity pairs* (`X -> X`) are owners too: the in-place scan (`scan_in_place_young`) needs the same lookup.
>   4. *VM (cross-lane):* `young_marker_follows_side_tables()` returns `true` for a moving requested cycle once 1-3 exist, so the root gatherer drops the unconditional rows; `roots::conditional_loader_metadata` needs no change beyond that predicate.
> - **P-C (the old half of a moving requested major must seed from the true roots).** Phase 5's `major_gc_finalizing` keeps the legacy seed (every young survivor an old root), which would undo gcd d4/n's fix for every `System.gc()`. On a Cheney cycle the survivors ARE the true-root closure (only what the roots reach is copied), so the Phase 5 seed can be the to-space and promotion destinations instead of "every young object" -- but side-table values of moved owners must join it (gce e1/c's `side_table_values_of_moved_owners` now feeds exactly those rows to Phase 5's major, which is the half of P-C that was missing for P-A's rows). Lane c's code (`sweep_old_gen_non_moving_impl` / `major_gc_finalizing`).
> - **P-B (a refused moving `System.gc()` collects nothing on that call).** Once P-C exists the refusal can run the same true-root old sweep the non-moving path runs today ("Phase 5 alone", proposal P3 of the young6 review), so a refusal is never weaker than today's default.
> - Gate unchanged (the ABBA below, plus P-C's `GenR4W6JitOomRootProbe -Xmx64m` and `GenR4W4NativeStringOomProbe` with `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG=1` printing the default arm's lines).

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`): unchanged -- STILL TRUE by default; the flip needs P-A, P-B and P-C.** No code on the `explicit_full_gc` path changed in waves d6-d7; `GenR4w2SystemGcLoopProbe` stays =HS in the d7 battery (`syscg_loop`). The d7 `d3m_cursor_*` rows confirm the default requested major still takes the non-moving cycle (`oldsz_compactions=0`, `oldsz_moving_major_vetoes=0` on all 20 runs).

## STATUS (2026-09-28, gcd d5/s): STILL TRUE by default; no code change; the flip now needs P-A, P-B AND a new P-C

Re-read on `d916d1c40`. `collect_garbage_inner_with_pins` still ORs
`explicit_full_gc = gc_quiescence::explicit_full_gc_sweeps_young_in_place()`
(`major_gc_requested() && !gc_flags().gc_system_gc_moving_young`) into
`divert_non_moving`, and the same predicate still feeds
`young_marker_follows_side_tables` and `roots::conditional_loader_metadata`
(R1 holds). `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG` is still the opt-in.

What is left before `System.gc()` can move young on the default path:

- **P-A** (unchanged): a moving `System.gc()` roots every mirror and loader's
  metadata, so it cannot unload a class, until the Cheney closure follows
  `mirror_pin` / `metadata_pin` from copied, promoted and old owners.
- **P-B** (unchanged): a refused moving `System.gc()` collects nothing on
  that call ("Phase 5 alone" is proposal P3 of the young6 review).
- **P-C (new, gcd d4/n).** Every requested major today is the NON-moving
  path's old sweep, and that is where d4/n's true-root young seed lives
  (`run_non_moving_young_cycle` arms `TrueRootMajorScope`). A MOVING
  `System.gc()` would run the old half in Phase 5 (`major_gc_finalizing`),
  which keeps the legacy seed (every young survivor an old-gen root). So
  flipping the term would silently undo the fix of
  `gcd-d4j-stw-major-keeps-dead-old-data-through-young-nepotism-20260928.md`
  for every `System.gc()`, and bring back the OOME-probe failures it closed.
  P-C needs the Phase 5 major of a requested cycle to seed from the true
  roots too (old-gen lane), or the moving cycle's own survivors to BE the
  true-root set (they are, on a Cheney copy: only what the roots reach is
  copied, so the seed could be the to-space and promotion destinations --
  a design note for the old-gen lane, not verified here).

Gate unchanged: `CRATONVM_DBG=gc-stats,gcpause cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4w2SystemGcLoopProbe 200`,
ABBA with and without `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG=1`: the default
arm reports `nonmoving-explicit-full-gc` on the `System.gc()` cycles, the
opt-in arm a `moving-*` reason with `skipped=0`, both the probe's checksum
line. Add for P-C: `GenR4W6JitOomRootProbe -Xmx64m` and
`GenR4W4NativeStringOomProbe` with `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG=1`
must print the default arm's lines.

## Previous STATUS (2026-09-26, gen r5w1/young5): STILL TRUE by default; no code change; the flip stays gated on P-A / P-B

Re-verified against `9e252c8b2`:

- `collect_garbage_inner` still computes
  `explicit_full_gc = gc_quiescence::explicit_full_gc_sweeps_young_in_place()`
  (`= major_gc_requested() && !gc_flags().gc_system_gc_moving_young`) and
  ORs it into `divert_non_moving`; its decision arm still reports
  `nonmoving-explicit-full-gc`. The same one predicate still feeds
  `young_marker_follows_side_tables` and `roots::conditional_loader_metadata`,
  so the round-2 coupling (R1) holds.
- `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG` (`types/src/flags.rs`, `present`,
  default off) is the opt-in; with it and `CRATONVM_GEN_PINNED_YOUNG_COPY`
  a JIT-warm `System.gc()` can take the pinned copy (term 4 alone), as wave 6
  recorded.
- Nothing this lane changed touches the term or Phase 5. The walker
  unification (drift page) runs under every non-moving `System.gc()` and keeps
  every decision; the per-heap commit screen changes nothing on this path.

Why no fix landed: the preconditions wave 4 set are unchanged and still unmet.
P-A (a moving `System.gc()` roots every mirror and loader's metadata, so it
cannot unload a class) needs the Cheney closure to follow `mirror_pin` /
`metadata_pin` from copied, promoted and OLD owners (`gen_evac.rs` plus the
serial closure plus a seed from old owners), which is a design change with
class-unloading semantics, not a contained fix. P-B (a refused moving
`System.gc()` collects nothing on that call) needs "Phase 5 alone" (the major
without a young half), which is proposal P3 of the young6 review. Flipping
the default before both is a semantic change on every `System.gc()`.

Probe (unchanged, the A/B that gates the flip; orchestrator):
`CRATONVM_DBG=gc-stats,gcpause cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4w2SystemGcLoopProbe 200`,
ABBA with and without `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG=1`: the default arm
reports `nonmoving-explicit-full-gc` on the `System.gc()` cycles; the opt-in
arm must report a `moving-*` reason with `skipped=0`, and both print the
probe's checksum line.

---

Slug: `gengc-core-system-gc-forces-non-moving` · Round: gengc-round1, lane A1 (core)
Owner file: `gc/src/gen_heap.rs` · Filed 2026-09-20

## What is wrong

`collect_garbage_inner` computes

```rust
let explicit_full_gc = crate::gc_quiescence::major_gc_requested();
...
let divert_non_moving = (has_conservative_roots && !moving_young)
    || unrewritable_conservative_jit_roots
    || honor_promotion_oom_risk
    || divert_for_incomplete_moving_coverage
    || explicit_full_gc
    || gpu_relocation_forbidden;
```

`explicit_full_gc` is an **unconditional** term. It does not consult JIT frames,
the per-cycle coverage proof, generation occupancy, or anything else. So a
`System.gc()` always takes the non-moving in-place sweep — on a `--nojit` run,
on a fully JIT-quiescent pause, on a cycle whose coverage proof passed cleanly.

Two consequences follow, and the second is not obvious from the branch:

1. **`System.gc()` never compacts the young generation.** It marks, sweeps dead
   young spans into the from-space free list, and (threshold- or
   request-gated) sweeps old gen in place. The bump cursor never retreats,
   because the non-moving sweep cannot relocate a survivor.
2. **Phase 5's `take_major_gc_request()` on the moving path is nearly dead
   code.** `run_non_moving_young_cycle` consumes the request first, and the
   only way to reach Phase 5 with `major_requested == true` is
   `CRATONVM_DBG_FORCE_MOVING`. The elaborate compose-`pointer_map`-with-
   `compact_map` machinery right below it therefore runs almost only on the
   occupancy-driven path.

## Why it matters

`docs/gc-tuning.md` names the `[moving-young] fallback` rate as *the* throughput
problem of this backend. This term is one of the two that dominate it (the other
is `unrewritable_conservative_jit_roots` — see
`gengc-core-moving-young-cycle-counter-inert-FIXED-20260923.md`).

It bites hardest on exactly the workloads that are used to measure GC: a
benchmark harness built out of `System.gc()` calls measures the *non-moving
sweep*, whatever `-XX:+UseGenerationalGC`'s young generation is documented to
be. `docs/gc-tuning.md` already records the same trap for ZGC ("a benchmark
built out of `System.gc()` calls measures the stop-the-world path however this
is configured"); the generational equivalent is undocumented.

It also interacts badly with the young-gen sizing loop. A `System.gc()`-heavy
workload gets no compaction, so the from-space fragments monotonically, so the
selective-promotion defragmentation escalation
(`young_arena_needs_defragmentation`) becomes the *only* thing that ever drains
young — and that escalation only fires once the largest free hole is below
64 KiB, i.e. after the arena has already degenerated.

## Evidence

* `gc/src/gen_heap.rs`, `collect_garbage_inner`: the `explicit_full_gc` binding
  and its appearance in `divert_non_moving` (see the `gengc-round1-core` note
  added to the "what remains is exactly two diversions" comment block).
* `gc/src/gen_heap.rs`, `run_non_moving_young_cycle`:
  `let major_requested = crate::gc_quiescence::take_major_gc_request();` — the
  comment there says the request must be consumed on this path "because the
  moving Phase-5 check below is skipped", which is the same fact from the other
  side.
* The stated justification is one sentence at the binding:

  > System.gc() requests an old-gen-inclusive cycle. Route that cycle through
  > the non-moving marker so it can follow collection-overlay edges from live
  > owners instead of globally rooting every overlay.

  That is an argument about the **old-gen marker's overlay precision**, not
  about whether the **young** half may copy. The moving path already publishes
  young relocation to the overlay providers before `major_gc` consults them
  (`crate::external_roots::remap_external_roots(&pointer_map)` immediately
  before the `Self::major_gc(..)` call), precisely so the same-cycle old marker
  sees post-copy addresses.

## What a fix would look like

The cheap, separable version — **split the term in two**:

* Keep `explicit_full_gc` as a reason to run the OLD-generation half of the
  cycle (it already is: Phase 5's `major_requested`).
* Stop letting it decide the YOUNG half. A `System.gc()` on a cycle that would
  otherwise have compacted should compact, then run `major_gc` as Phase 5
  already does.

Concretely: remove `explicit_full_gc` from `divert_non_moving`, and leave
`take_major_gc_request()` where it is on each path (both already consume it
exactly once per cycle). The decision-reason arm
`dr::NON_MOVING_EXPLICIT_FULL_GC` then becomes the fall-through for
`gpu_relocation_forbidden && explicit_full_gc`, and should be re-pointed or
retired.

The reason this is filed rather than done: the overlay-precision argument may be
load-bearing for `old_gen_gc`'s **mark** phase in a way the young half's
`remap_external_roots` does not cover, and separating them safely needs the
`RefCheck` / `RefCheckOld` probes plus a collection-overlay workload
(native-collections `Hash*`/`ArrayList` overlays), which this session could not
run.

## How to verify

1. **Engagement first.** On a `System.gc()`-driven probe under
   `-XX:+UseGenerationalGC`, `gc_metrics::collector_decision_report()` must stop
   reporting `NON_MOVING_EXPLICIT_FULL_GC` and start reporting
   `MOVING_NO_JIT_FRAMES` / `MOVING_WITH_PROVEN_JIT_COVERAGE`. A change that
   leaves the histogram unmoved has changed nothing.
2. **Correctness.** `RefCheck`, `RefCheckOld`, `ChurnCheck`, `CopyChurn` and
   `MTChurn` from the probe kit at `-Xmx256m`, diffed against a real JDK — the
   reference protocol and the overlay edges are the two things this term
   protects.
3. **Collections overlay specifically.** A workload that puts a
   native-collection overlay's backing array through a `System.gc()` while the
   only reference to it is provider-held. `CRATONVM_DBG_WEAKREF=1` plus the
   `old-gen mark: rejecting external-overlay(BFS owner)` warning are the two
   signals that the overlay seed went stale.
4. **Throughput.** `bench/BinT.java` (self-checking) and
   `bench/OldGenRsetProbe`, A/B on one binary, with `CRATONVM_DBG=gcpause` to
   confirm the pause shape changed from `gcphase` (sweep) to `cheney_drain`
   (copy) rather than merely moving.

---

## Resolved 2026-09-20 — NOT CLOSED. The proposed fix is unsafe as written.

Reviewed by gengc-round2 lane B1 (`core2`). The term stays. What follows is
the reason, which is not the reason this page gives for hesitating.

### The analysis above is incomplete, and the missing half is decisive

This page treats `explicit_full_gc` as a private policy of the collector,
whose only external commitment is the overlay-precision argument quoted in
"Evidence", and concludes that the argument is about the old-gen marker rather
than about whether the young half may copy. That is a correct reading of the
comment and an incomplete reading of the system.

`explicit_full_gc` is a **promise published to the VM's root gatherer, and
consumed before this function decides anything**:

* `gc_quiescence::young_marker_follows_side_tables()` — "May the young marker
  follow side tables instead of rooting their contents unconditionally?" — is
  ```rust
  if dbg_force_moving { return false; }
  if major_gc_requested() { return true; }          // <-- this term
  !moving_young_enabled() && (is_active() || unregistered_jit_frame_on_stack())
  ```
  Its own doc calls the `major_gc_requested()` arm "certain, and independent of
  moving-young", and states the stake in as many words: *"A false negative
  merely costs one extra conservative root; a false positive would DROP a live
  root."* It is what lets `VmHeap::mirror_pin_deferrable` leave a class mirror
  out of the unconditional root set.
* `vm::memory::roots::conditional_loader_metadata` asks the same question for
  loader metadata, and its Generational arm is
  `is_active() || major_gc_requested()`.

Both are evaluated while roots are being gathered — before the collector runs,
let alone decides. Delete `explicit_full_gc` from `divert_non_moving` and a
`System.gc()` under moving-young RELOCATES against a root set that was
deliberately thinned on the promise that the cycle would sweep in place. The
failure mode is not hypothetical: `young_marker_follows_side_tables`' doc
records the last time these two halves drifted apart, when the `!moving_young`
guard was wrongly factored across the `major_gc_requested()` disjunct and
"re-open[ed] `TestDefaultInstanceManager.testClassUnloading` for the third
time".

### What that means for this page

The gap is **real and unchanged** — every `System.gc()` still forces the
non-moving young sweep, a `System.gc()`-driven benchmark still measures the
sweep, and consequence 2 (Phase 5's `take_major_gc_request()` being nearly
dead code on the moving path) still holds. What is refuted is the claim that
the fix is "cheap, separable" and contained to `gen_heap.rs`.

The real shape of the fix is a three-file, one-commit change:

1. `gc/src/gc_quiescence.rs` — `young_marker_follows_side_tables` must stop
   returning `true` for `major_gc_requested()` alone, i.e. the root gatherer
   must root side-table contents unconditionally on a `System.gc()` that may
   move. That is a throughput cost on every explicit full GC, paid to buy the
   compaction.
2. `vm/src/memory/roots.rs` — `conditional_loader_metadata`'s Generational arm
   likewise.
3. `gc/src/gen_heap.rs` — only then may the term leave `divert_non_moving`.

Ordering matters: step 3 alone is a live-object reclamation bug. Steps 1-2
alone are a pure (small) pessimisation and are safe to land first.

A code comment naming this coupling now sits on the `explicit_full_gc` binding
in `collect_garbage_inner`, so the next reader does not rediscover it the hard
way.

### Verification this still needs, on top of the page's own list

Before and after steps 1-2, `TestDefaultInstanceManager.testClassUnloading`
and any mirror-unloading workload, because those are what the deferral exists
for and what breaks when it is withdrawn or wrongly kept.

---

## 2026-09-23 round 4 re-check (gengc-round4 lane A, `move`) — STILL OPEN, unchanged

Re-verified against current `collect_garbage_inner`: `explicit_full_gc =
gc_quiescence::major_gc_requested()` is still an unconditional term of
`divert_non_moving`, and its decision arm still reports
`NON_MOVING_EXPLICIT_FULL_GC`. The round-2 finding stands and is still the
reason not to delete the term: `young_marker_follows_side_tables()` and
`vm::memory::roots::conditional_loader_metadata` both thin the root set on
`major_gc_requested()` BEFORE the collector decides, so a moving `System.gc()`
would relocate against a root set that assumed an in-place sweep.

One thing round 4 adds, which the page's "consequence 2" understates: the
moving path's Phase 5 is not merely "nearly dead" for `System.gc()` — on the
moving path the only same-cycle major is occupancy-driven, and round 4 found and
fixed a correctness defect on exactly that path (a dead finalizable promoted by
Phase 2.5 was freed by the same cycle's major; see
`docs/internal/reviews/gengc-round4-move-20260923.md`). Any change that sends
`System.gc()` through the moving path will make that path hot for finalizer-
heavy programs, so the new regression test
`a_finalizable_promoted_by_resurrection_survives_the_same_cycle_major` belongs
in the verification list of whoever lands steps 1-3.

**Sharper first step (unchanged in substance):** land steps 1-2 alone
(`gc_quiescence.rs`, `vm/src/memory/roots.rs`) behind no flag, measure the
pessimisation with `CRATONVM_DBG=gcpause` on a `System.gc()`-heavy probe, and
only then remove the term. A counter first would help: the decision histogram
already counts `nonmoving-explicit-full-gc`, so the size of the prize is
readable from any `[GC] decision histogram:` line today.

---

## 2026-09-23 round 4 wave 2 (lane `youngpolicy`) — steps 1-3 landed as an OPT-IN; default unchanged

The three-file change round 2 specified is in, behind one declared flag so the
default stays byte-for-byte the same and the A/B is one binary:

* `gc/src/gc_quiescence.rs`: new `explicit_full_gc_sweeps_young_in_place()` =
  `major_gc_requested() && !gc_flags().gc_system_gc_moving_young`.
  `young_marker_follows_side_tables()` now asks it instead of
  `major_gc_requested()`, so `VmHeap::mirror_pin_deferrable` and the
  collection-overlay root scan in `vm/src/memory/native_roots.rs` follow it.
* `vm/src/memory/roots.rs::conditional_loader_metadata`: the Generational arm
  is `is_active() || explicit_full_gc_sweeps_young_in_place()`.
* `gen_heap::collect_garbage_inner`: `explicit_full_gc` is that same predicate.

ONE predicate for the promise and its keeper, so they cannot drift. The flag is
`CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG` (token `CRATONVM_GC=system-gc-moving-young`,
`parse::present`, default off). With it set, a `System.gc()` roots mirrors,
loader metadata and collection overlays unconditionally, takes the moving path
when no other term diverts, and still runs Phase 5's major (the request is
consumed there, or by `run_non_moving_young_cycle` if another term diverts).

Test: `gen_heap::tests::a_system_gc_compacts_young_only_under_the_opt_in`
(default: `nonmoving-explicit-full-gc`, survivor unmoved; opt-in: moving
decision, survivor relocated with its payload, `major_gc_count` advanced,
request consumed once, root-gatherer predicate flipped).

**Known cost under the flag (by design, and why it is not the default yet):** a
`System.gc()` cannot unload a class or free an overlay-only collection on that
cycle, because the side tables are rooted. `TestDefaultInstanceManager.
testClassUnloading` and overlay-reclamation probes are expected to behave
differently under it; that is the price being measured.

**Also noted:** with the flag on, a moving `System.gc()` that REFUSES (to-space
undersized, walk incomplete, T-3 tail) returns before Phase 5, so the request
stays pending and the next collection on that thread runs the major — the
"refused = the same cycle, retried" semantics, not a leak into an unrelated
cycle. With the flag off this cannot happen (the non-moving path consumes it).

**Still open:** whether to flip the default. A/B on one binary, interleaved
ABBA, `-XX:+UseGenerationalGC -Xmx256m`, `CRATONVM_DBG=gc-stats,gcpause`:
`tools/bench/BinT 14` (checksum `327670`), a `System.gc()`-looping probe (a
20-line loop that allocates ~1 MB of short-lived objects then calls
`System.gc()`, 200 iterations), `RefCheck`/`RefCheckOld`,
`TestDefaultInstanceManager.testClassUnloading`,
`probes/ReachableFinalizeProbe.java`, and
`a_finalizable_promoted_by_resurrection_survives_the_same_cycle_major` in the
unit suite. Engagement signal: `nonmoving-explicit-full-gc` leaves the
`[GC] decision histogram:` and `moving-no-jit-frames-live` rises by the same
count; pause shape moves from `gcphase` rows to `cheney_drain`.

---

## 2026-09-23 round 4 wave 3 (lane `young2`) — re-checked; the remaining part is the default flip

Re-read against `85646aa9a`. The opt-in is intact and its one predicate is
still shared by the promise and the keeper
(`explicit_full_gc_sweeps_young_in_place()` in `gc_quiescence.rs`, read by
`young_marker_follows_side_tables`, `roots.rs::conditional_loader_metadata`
and `collect_garbage_inner`'s `explicit_full_gc`). Nothing in the young lanes
of wave 3 changes that coupling. Two things were checked for the opt-in path
and found sound:

* **A refused moving `System.gc()` under the opt-in.** There is no mid-flight
  divert any more (`run_non_moving_young_cycle` has one caller, the up-front
  divert; an incomplete object-start walk REFUSES the cycle), so the only
  late exits are the three refusals, which leave the request pending for the
  retry exactly as wave 2 describes.
* **Phase 5's debug line** (`[DBG_MIRRORPIN] Phase5 … will_run_major=`) used to
  recompute the bare 75 % expression; wave 3 made it print the trigger's actual
  answer (`major_trigger_decision`, asked once). Under the opt-in that line is
  what tells a probe whether a moving `System.gc()` really ran its major.

**Still open, by rule:** flipping the default is a policy change on every
`System.gc()`, and it has no measurement yet. The wave-2 A/B stands as the
acceptance; add to it, for the class-unloading cost, a count of
`System.gc()` cycles whose mirror/metadata propagation found something to
unload (the youngpolicy review's proposal P2 middle road needs exactly that
bit before it can be designed). No code in this lane moves until that A/B is
run.

---

## 2026-09-24 round 4 wave 4 (lane `young4`) — every non-moving reason re-checked; the flip is prepared, with two preconditions

Re-read against `fc68d6dc1`. The orchestrator measures
`CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG=1` against the default this wave; this
block is what that A/B has to be read against. **No code changed for this
page in wave 4** (the lane's `collect_garbage_inner` hunks do not touch the
`explicit_full_gc` term or Phase 5).

### Every reason `System.gc()` was non-moving, against today's code

| # | Reason | Status today |
|---|---|---|
| R1 | The root gatherer thins mirror / loader-metadata / collection-overlay roots on the in-place promise (round 2's finding). | **Closed by construction.** ONE predicate, `gc_quiescence::explicit_full_gc_sweeps_young_in_place()`, is read by the term in `collect_garbage_inner`, `young_marker_follows_side_tables` (→ `VmHeap::mirror_pin_deferrable` and `native_roots::scan_collection_overlays`), `roots::conditional_loader_metadata` and `roots.rs` step 14w (`metadata_weak_mode_needs_in_place_promise`). With the flag on, all of them root the side tables unconditionally on a `System.gc()`. Checked every reader of `major_gc_requested()` outside `gc_quiescence.rs`: the only one left is `native_roots.rs`'s `CRATONVM_DBG_OVERLAY_GATE` debug print. |
| R2 | Overlay precision of the same-cycle OLD marker. | **Closed.** The moving path calls `remap_external_roots(&pointer_map)` before `major_gc`, and under the flag the overlays are rooted anyway (R1). |
| R3 | Phase 5's `take_major_gc_request()` was dead on the moving path. | **Live and correct.** Phase 5 takes the request unconditionally (no `||` short-circuit) and `major_trigger_decision(.., major_requested)` forces the major. |
| R4 | The same-cycle major freed a finalizable promoted by Phase 2.5 (round 4 `move`). | **Fixed**; `a_finalizable_promoted_by_resurrection_survives_the_same_cycle_major` pins it. Wave 4's serial promotion buffer retires its tail after Phase 2.5b, before Phase 5, so the major never walks an unreleased buffer. |
| R5 | A REFUSED moving cycle (to-space undersized, incomplete object-start walk, T-3 tail) returns before Phase 5. | **Still true, and it is precondition P-B below.** With the flag off this cannot happen: the non-moving path never refuses. With it on, the request stays pending and the NEXT collection on that thread runs the major — so a `System.gc()` whose moving young half refuses returns having collected NOTHING, young or old. |
| R6 | Class unloading on `System.gc()`. | **Regresses under the flag, and it is precondition P-A below.** The moving Cheney closure follows `loader_pin` (instance → defining loader) but not `mirror_pin` / `metadata_pin`; the non-moving marker follows all three. So on a moving `System.gc()` the side tables must be ROOTED (R1), which keeps every class mirror and loader's metadata alive through that cycle — a `System.gc()` can no longer unload a class. HotSpot's full GC does. |
| R7 | HIB-CV-22/32/33: the non-moving sweep on a PRECISE-root cycle lacks the conservative over-marking it relies on. | **An argument FOR the flip.** Today every no-JIT `System.gc()` runs exactly that configuration (explicit_full_gc diverts a cycle with no conservative roots); the flag removes it from the default path. |
| R8 | GPU relocation veto, promotion-OOM risk, coverage proof, term 4. | Unchanged and independent: they still divert a `System.gc()` on their own (the reason chain names them ahead of `nonmoving-explicit-full-gc`). |

### The one-line flip

`types/src/flags.rs`, in `VmFlags`' constructor:

```rust
-            gc_system_gc_moving_young: present(src, "CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG"),
+            gc_system_gc_moving_young: on_unless_zero(src, "CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG"),
```

plus, in the same commit, the `flag_groups.rs` row gaining the opt-out
(`off_key: Some("CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG")`, `off_word: Some("0")`,
or whatever shape `on_unless_zero` flags use there), the field's doc, and the
regenerated `docs/config/flag-inventory.md` / `docs/flag-tokens.md` (generated —
not hand-edited). `a_system_gc_compacts_young_only_under_the_opt_in` then
needs its two arms swapped (default = moving; `=0` = in place).

### Preconditions (do not flip on the throughput A/B alone)

* **P-A — class unloading.** Either teach the Cheney closure to follow
  `mirror_pin` / `metadata_pin` from every copied or promoted OWNER (the way it
  follows `loader_pin`, and seeded from OLD owners the way the non-moving
  marker's `seed_mirror_pins_of_old_owners` does), after which
  `young_marker_follows_side_tables` may return `true` on a moving cycle too;
  or accept, in writing, that `System.gc()` stops unloading classes. The A/B
  must include `TestDefaultInstanceManager.testClassUnloading` and a
  `WeakReference<ClassLoader>` cleared-after-`System.gc()` check.
* **P-B — a refused moving `System.gc()` must still collect.** Retry the same
  call on the non-moving path when the moving half refuses on a `System.gc()`
  (the side tables are ROOTED under the flag, so the non-moving marker only
  over-retains; R7's hazard is about the precise root set, and a refusal
  after a precise root gather would put it back — so the retry is only safe
  for the to-space-undersized refusal, and the incomplete-walk refusal should
  instead run Phase 5 alone). Until then, count it: the refusal reason codes
  in the decision histogram (`skipped=`) on a `System.gc()`-driven probe must
  read 0 in the A/B, or the flip changes `System.gc()` semantics.

### The A/B (orchestrator)

`CRATONVM_DBG=gc-stats,gcpause -XX:+UseGenerationalGC -Xmx256m`, ABBA:
`tools/bench/GenR4w2SystemGcLoopProbe 200` (checksum as in its header; engagement:
`nonmoving-explicit-full-gc` → ~0, `moving-no-jit-frames-live` up by the same
count, `skipped=0`), `BinT 14` (`327670`), `RefCheck` / `RefCheckOld`,
`probes/ReachableFinalizeProbe.java`, `TestDefaultInstanceManager.testClassUnloading`
(expected to differ under the flag until P-A lands — that difference IS the
finding). Flip only with P-A and P-B settled.

---

## 2026-09-24 round 4 wave 6 (lane `young6`): STILL OPEN; the JIT-warm half is already wired, the flip is not

Re-read against `28f4acd3a`. **No code changed for this page in wave 6.**

* **Default: yes, `System.gc()` still forces the non-moving sweep.**
  `explicit_full_gc = gc_quiescence::explicit_full_gc_sweeps_young_in_place()`
  is still a term of `divert_non_moving`, `true` for every `System.gc()`
  unless `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG` is set. The one-predicate
  coupling with the root gatherer (R1) is intact.
* **Under the opt-in, a JIT-warm `System.gc()` already takes the pinned-page
  copy (term 4's mechanism).** The pinned-copy candidate test is
  `term4_alone = unrewritable_conservative_jit_roots && ... && !explicit_full_gc && ...`;
  with the flag on, `explicit_full_gc` is `false`, so a `System.gc()` whose
  only other diverting term is term 4 goes down the pinned in-place cycle
  when `CRATONVM_GEN_PINNED_YOUNG_COPY` is also set. That cycle reaches Phase
  5 (the swap is skipped, Phase 5 is not: `take_major_gc_request` and
  `major_trigger_decision` run for `in_place_cycle` too), so the request is
  consumed and the major runs. So "a proper full collection with moving
  young, as HotSpot does", JIT-warm included, is exactly
  `CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG=1 CRATONVM_GEN_PINNED_YOUNG_COPY=1`.
  Both are opt-in pending their own A/Bs.
* **Why the default was not flipped here.** Wave 4's preconditions are
  unchanged and still unmet: P-A (a moving `System.gc()` roots every class
  mirror and loader's metadata for the cycle, so it cannot unload a class;
  the Cheney closure would have to follow `mirror_pin` / `metadata_pin` from
  copied, promoted and old owners, which is `gen_evac.rs` and the serial
  closure together) and P-B (a REFUSED moving `System.gc()` collects
  nothing that call). On P-B, by reading this wave: of the three refusals,
  the to-space-undersized one is unreachable after the equalisation above
  it (it needs a non-empty to-space at entry), and the two reachable ones
  (incomplete object-start walk, T-3 reserved TLAB tails) are exactly the
  cases whose own comments say the non-moving sweep is NOT a safe fallback on
  a precise-root cycle — so the retry the page sketches would be dead code on
  the only arm where it is safe. The honest P-B is "run Phase 5 alone on a
  refused explicit request", which needs the major without a young half; it is
  filed as proposal P3 in `docs/internal/reviews/gengc-round4-w6-young6-20260924.md`.
* **The measurement is still the gate**, and it is the wave-4 A/B unchanged;
  add the JIT-warm arm: `GenR4w2SystemGcLoopProbe 200` (JIT on, so its loop
  compiles) with both flags, where the `System.gc()` cycles must read
  `moving-pinned-pages` or `moving-*` (not `nonmoving-explicit-full-gc` or
  `nonmoving-unrewritable-conservative-jit-roots`) in the
  `[GC] decision histogram:` line. (`GenR4W4JitWarmDivertProbe` calls no
  `System.gc()` and does not exercise this.)
