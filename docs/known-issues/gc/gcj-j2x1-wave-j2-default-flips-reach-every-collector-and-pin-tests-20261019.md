# Wave j2's four default flips: two readers are not Generational-only, and seven tests pin OFF

> **STATUS (2026-10-19, gcj j3/x2): (i) and (ii) FIXED IN CODE, PENDING BUILD/PROBE; (iii), (iv) open
> (end-of-round regeneration; naming nit).** (i) the dedupe arm records its arming thread and only a gather
> on that thread consumes it; a foreign gather is whole, leaves the arm and is counted
> (`ThreadRegistry::initiator_skip_foreign_gathers`, `foreign_gathers=` on the `[rootprof]` line); test
> `gcj3x2_a_foreign_threads_gather_neither_consumes_nor_applies_the_arm`. (ii) the probe walk is counted
> (`stw_probe_census`: walks, abandoned, visited, ns) with a `[watchref] stw-probe:` line per walk under
> `CRATONVM_DBG_WATCHREF=1`; test `gcj3x2_the_stw_probe_walk_counts_its_cost_and_its_abandonment`; the
> fail-first row for the budget's parity loss is `Gcj3X2StwProbeBudgetProbe big`; the cost cut is proposal
> `gcj-j3x2-proposal-stw-probe-walks-only-what-can-die-20261019`. Rows: `j3-x2-report.md` blocks SB, DD.

> **STATUS (2026-10-19, gcj j2/x1, re-checked against the flip commit `265469f02e`): NARROWED.** The
> orchestrator's flip commit resolves sections 1, 2a and 2b as written below: every reader is now
> `runtime_flag_default_on`, the dedupe and STW-refs readers are gated on `is_generational()` (G1 / ZGC keep
> `runtime_flag_on`), every row carries `off_word: Some("0")`, and the tests of section 3 were updated
> (`gci_i22dr_off_...` asserts unset ON; `gcj1jn_...` unset ON / `=0` OFF; the dedupe source pin names
> `dedupe_initiator_snapshot_enabled(shared)`; new `gcj2_stw_undiscovered_refs_strong_is_default_on_for_generational_only`).
> STILL OPEN: (i) the dedupe arm's per-thread hardening (end of 2a); (ii) the STW probe's per-pause walk
> cost and its silent budget abandonment (end of 2b) -- now on the DEFAULT Generational path; (iii) the
> surface fixture (section 4); (iv) a naming nit: `gci_i16fr_stw_undiscovered_refs_strong_is_default_off`
> (`gc_and_alloc.rs:30336` on the flip tree) now pins the G1 / ZGC reader `stw_undiscovered_refs_strong_enabled`
> only, as does `before_collection` (test-only) -- correct, but the name reads as the Generational default.
> Checked on the flip commit: the new `runtime_flag_default_on("CRATONVM_GEN_TLAB_REFILL_SMALLEST_WHOLE_BLOCK")`
> line is 99 columns (rustfmt-legal); the dedupe source pin's second assert
> (`runtime_flag_on("CRATONVM_GC_DEDUPE_INITIATOR_SNAPSHOT")`) still matches its `else` arm.

> **STATUS (2026-10-19, gcj j2/x1): OPEN -- for the orchestrator, who applies the flips.** Found by
> reading tree `c82304324b` (lane x1 did not edit the four readers: LANE-RULES "do not edit their readers'
> default logic"). Each item below has the exact diff; none is applied on `gcj2-x1`.

The four switches the orchestrator flips in wave j2 are meant to be "Generational only, each with its
`=0` kill". Three facts decide whether a flip is what it says.

## 1. Flipping `off_word` alone turns nothing on

`types/src/flag_groups.rs` `off_word` is metadata: `flag_groups::apply` reads it only to expand
`CRATONVM_GC=-token` (`types/src/flag_groups.rs` ~12282-12293). Every one of the four readers is
`runtime_flag_on` (`types/src/flags.rs:6254`, `None => false`), so each flip needs the reader changed to
`runtime_flag_default_on` (`flags.rs:6298`, `None => true`, off for `0/false/off/no`) **and** the row's
`off_word: Some("0")`. Note the value-rule change that comes with it: an EMPTY value (`KEY=`) is OFF under
`runtime_flag_on` and ON under `runtime_flag_default_on`.

| switch | reader | row |
|---|---|---|
| `CRATONVM_GEN_TLAB_REFILL_SMALLEST_WHOLE_BLOCK` | `gc/src/gen_heap.rs:8460` (latched per heap at `:12881`) | `flag_groups.rs:9648` |
| `CRATONVM_GC_DEDUPE_INITIATOR_SNAPSHOT` | `vm/src/runtime/interpreter/gc_and_alloc.rs:6156` | `flag_groups.rs:8655` |
| `CRATONVM_STW_UNDISCOVERED_REFS_STRONG` | `gc_and_alloc.rs:17096` | `flag_groups.rs:6774` |
| `CRATONVM_JNI_FOREIGN_ALLOC_LADDER` | `vm/src/native/jni.rs:13793` (latched per VM, `JniSwitches::read_flags` ~`:3060`) | `flag_groups.rs:8661` |

## 2. Two of the four reach G1 and ZGC (ZGC is the default collector)

### 2a. `CRATONVM_GC_DEDUPE_INITIATOR_SNAPSHOT`

`arm_initiator_snapshot_skip` (`gc_and_alloc.rs:6194`) tests only the switch, and its caller
`run_collection_pause` (`gc_and_alloc.rs:5451`) is the shared stop-the-world door of every backend. A
plain `runtime_flag_default_on` flip turns the dedupe on for ZGC and G1. By reading the effect there is
only the removal of a duplicate root (step 11's range is already conservative, `vm/src/memory/roots.rs:2568`;
ZGC's soft probe reads `roots[..census_collect_end]`, which holds step 11), but it is a G1/ZGC behaviour
change the round's rules forbid unmeasured. Gated diff (an explicit `=1` stays honoured everywhere, as the
opt-in is today):

```diff
-fn dedupe_initiator_snapshot_enabled() -> bool {
-    cratonvm_types::flags::runtime_flag_on("CRATONVM_GC_DEDUPE_INITIATOR_SNAPSHOT")
+fn dedupe_initiator_snapshot_enabled(generational: bool) -> bool {
+    const NAME: &str = "CRATONVM_GC_DEDUPE_INITIATOR_SNAPSHOT";
+    if generational {
+        cratonvm_types::flags::runtime_flag_default_on(NAME)
+    } else {
+        cratonvm_types::flags::runtime_flag_on(NAME)
+    }
 }
@@ fn arm_initiator_snapshot_skip
-    if !dedupe_initiator_snapshot_enabled() {
+    if !dedupe_initiator_snapshot_enabled(shared.mem.heap.is_generational()) {
```

Source pin to update with it: `vm/src/threading/thread_registry.rs:16569-16572`
(`if !dedupe_initiator_snapshot_enabled()` / `runtime_flag_on("CRATONVM_GC_DEDUPE_INITIATOR_SNAPSHOT")`).

Hardening (not a flip blocker; low probability): the arm is a per-registry slot taken by the FIRST plain
`collect_all_root_snapshots()` after it is set (`thread_registry.rs:7335`). Any other gather between the
arm and the door's own (`vm/src/runtime/hprof.rs:1359`, the other doors at `gc_and_alloc.rs` ~25906,
26622, 28252, 28936, 29088, 29320) would consume it and leave the initiator's words out of a root set in
which they are NOT duplicated. The stop-the-world pause serialises them today, and the default-ON
parked-split arm has the same exposure. Fix: record `std::thread::current().id()` with the arm and take it
only on the arming thread (`ParkedSplitGather`, `thread_registry.rs` ~9095).

### 2b. `CRATONVM_STW_UNDISCOVERED_REFS_STRONG`

Reader `stw_undiscovered_refs_strong_enabled` (`gc_and_alloc.rs:17095-17097`), only production caller
`stw_probe_engaged` (`:17113-17115`), reached from the shared door (`:5534-5535`). A plain flip runs the
probe on G1 and ZGC, and on G1 it would also satisfy the "needs the STW probe" precondition of
`CRATONVM_G1_FULL_GC_KEEPS_UNDISCOVERED_REFS` (the i22 triage recorded a G1 S3 failure with the probe,
`docs/internal/gc-round-gci-20261011/i22-fl-report.md:73`). Gated diff:

```diff
-fn stw_undiscovered_refs_strong_enabled() -> bool {
-    cratonvm_types::flags::runtime_flag_on("CRATONVM_STW_UNDISCOVERED_REFS_STRONG")
+fn stw_undiscovered_refs_strong_on(generational: bool) -> bool {
+    const NAME: &str = "CRATONVM_STW_UNDISCOVERED_REFS_STRONG";
+    if generational {
+        cratonvm_types::flags::runtime_flag_default_on(NAME)
+    } else {
+        cratonvm_types::flags::runtime_flag_on(NAME)
+    }
 }
 fn stw_probe_engaged(shared: &SharedVm, door: GcDoor) -> bool {
-    stw_undiscovered_refs_strong_enabled() || g1_full_gc_keeps_undiscovered(shared, door)
+    stw_undiscovered_refs_strong_on(shared.mem.heap.is_generational())
+        || g1_full_gc_keeps_undiscovered(shared, door)
 }
```

The test-only `before_collection` (`gc_and_alloc.rs:17289`) then passes `&|| stw_undiscovered_refs_strong_on(true)`.

Cost note (measure before the flip, not a soundness item): `StwUndiscoveredProbe::before_collection_if`
(`gc_and_alloc.rs:17292-17318`) walks from EVERY registered finalizable, live ones included
(`finalizer_referent_addresses()`), on every pause that nulled a slot -- young pauses included -- up to a
`1 << 20` budget (`:17142`). Past the budget the probe is silently inactive, so the parity it buys is lost
with no counter. Rows: the vi16-fr C rows plus a young-pause A/B with a few thousand live finalizables over
a large graph; a census of abandoned probes would make the silent case visible.

The other two readers are Generational-only by construction: the TLAB switch is a `GenerationalHeap`
field (G1 has its own `refill_tlab_at_least`, `gc/src/g1.rs:34041`), and the foreign ladder tests
`is_generational()` first (`foreign_attached_alloc_may_collect`, `jni.rs:13822`).

## 3. Tests that pin the OFF default (red on the flip)

- `gc/src/gen_heap.rs:6175-6186` `gci_i22dr_off_the_refill_cuts_the_smallest_block_holding_the_request`:
  `for v in [None, Some("0")]` asserts OFF -> `[Some("0"), Some("off")]`; and
  `gci_i22dr_on_the_refill_takes_the_smallest_whole_hole_first` (`:6191`) should also assert `heap_with(None)` is ON.
- `vm/src/threading/thread_registry.rs:16572` (the reader's source text, see 2a).
- `vm/src/runtime/interpreter/gc_and_alloc.rs:30298` `gci_i16fr_stw_undiscovered_refs_strong_is_default_off`
  and `:31860` (`assert!(!stw_undiscovered_refs_strong_enabled())`): re-scope to `on(true)` / `!on(false)`.
- `vm/src/native/jni.rs:26819` (`assert!(!read(ladder, None).foreign_alloc_ladder, "unset: off")`, test
  `gcj1jn_the_three_opt_ins_are_latched_per_vm_and_off_by_default`, whose name also needs changing) and
  `jni.rs:26878-26881` (`(Generational, None) => (false, false)` becomes `(false, true)`; the G1 arm stays).
- Docs that say "opt-in": `docs/config/flag-inventory.md` (2022, 2205, 4621; regenerated), `docs/GC.md`
  (1118, 1862, 2095), `docs/gc-tuning.md:831`, and the doc comments at the readers.

## 4. The surface fixture misses two j1 keys

`types/tests/flag-surface.txt` has neither `CRATONVM_JNI_ARRAY_VM_LIMIT` nor
`CRATONVM_JNI_FOREIGN_ALLOC_LADDER`, so `types/tests/flag_surface.rs:86`
`inventory_matches_the_checked_in_surface` is red until the end-of-round regeneration (the orchestrator
already plans it; listed so it is not mistaken for a lane defect).

## What closes this page

The diffs of 2a / 2b applied with the flips, the tests of 3 updated, and the rows: every flipped switch's
deciding list re-run on the flip build with the G1 / ZGC rows KEPT (`XFILT` empty) for 2a / 2b -- the G1 /
ZGC rows must equal the pre-flip build's.
