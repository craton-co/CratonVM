# JIT round 14 wave 5, lane fixup2: proposals

Ranked. From closing `r14w4-review4-wave3-review-findings-FIXED-20260929.md`
section 2 (RV4-1, RV4-2, RV4-4, the lock-order decision, the ADDRESS NPE
message).

## FX2-1: key PGO loop profiles on `goto_w` back edges too

Benefit: `profile.rs` `LoopExtents::build_keyed` filters its
`bytecode_analysis::back_edges` set to `0x99..=0xa7 | 0xc6 | 0xc7` sources,
so a loop closed by `goto_w` (a body past 32 KiB, or javac's choice after a
large `switch`) gets no loop key and no `LoopTripProfile` under
`CRATONVM_TIER_PGO`; `hot_loop_ranges` (which decodes `goto_w` through
`loop_back_edge_header` since wave 5) then never sees it, and such a loop's
sites are priced by the profile-less fallback only when the profile has no
loop evidence at all. Cost: widen the filter to `0xc8` and check the
interpreter's back-edge recorder keys the `goto_w` pc the same way (its
`goto_w` arm). Risk: low (profile keys only). First step: a unit test in
`profile.rs` with a `goto_w` loop, asserting a key at its source pc.

## FX2-2: one "is this opcode a loop back edge" predicate crate-wide

Benefit: after RV4-4 the four `lib.rs` loop-range readers share
`loop_back_edge_header`, but `loop_analysis.rs` (738, 872, 1321),
`profile.rs` (837, 871) and `x64/bce.rs` (1469, 1927, 3545, 3657) still spell
their own opcode sets, and they disagree on `goto_w` (bce refuses a method
with one; `profile.rs` 837 drops it; `loop_analysis.rs` 738 keeps it). Moving
`loop_back_edge_header` into `bytecode_analysis` (as `loop_back_edge_target`)
and having each site either use it or state why it refuses would make the
disagreement a reviewed decision. Cost: small, multi-owner. Risk: low if each
site keeps its refusal. First step: list each site's set and its reason in a
table on a page.

## FX2-3: carry the sync-direct memo into the OSR doors

Benefit: RV4-1 memoises per `try_compile_request`; the optimizing OSR route
and the single-pass OSR door build their own requests, and a method that is
OSR-compiled and then entry-compiled asks the VM (and may eager-compile an
unpublished callee) once per door. A per-method memo of REFUSALS (not
answers: an answer carries a pin) on the VM side (`jit_bridge.rs`
`sync_direct_target`) would stop a callee whose eager compile failed from
being compiled again by the next door. Cost: a per-VM map keyed by
`(caller class, cp index)` with the callee's compile epoch, cleared on
redefinition. Risk: low-medium (staleness). First step: count
`sync_direct_compile_unpublished_callee` calls per callee under
`CRATONVM_DBG_JITC` on `R14SyncSpliceStatic`.

## FX2-4: memoise `gaussian_slots` with the `Random` layout

Benefit: `nextGaussian` on the real-field road still resolves
`haveNextNextGaussian` / `nextNextGaussian` by name per call
(`securerandom.rs` `gaussian_slots`), two more field-index resolutions on
every other draw. Cost: two more `usize` in `RandomLayout`. Risk: low. First
step: A/B `R14Fixup2RandomLayout`'s Gaussian loop with
`CRATONVM_RANDOM_LAYOUT_MEMO=0/1` after adding them.
