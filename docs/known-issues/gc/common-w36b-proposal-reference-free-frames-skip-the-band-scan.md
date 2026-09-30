# Proposal: skip the conservative band scan below `callee_saved_lo` for a reference-free compiled frame

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 49
> of 54).** Not built. **Gate:** behind an opt-in, a soak on all three
> collectors with `CRATONVM_DBG_VERIFY_OOP_MAPS=1`; excluded-word counts under
> the new `reference-free` label. **Size:** S (+ soak).

Status: PROPOSAL (gc-common w36-b, 2026-09-26). For the user's triage.

## What it would change

`vm/src/jit/conservative_roots.rs::scan_one_frame_filtered` (the default
band scan of a compiled frame) reads every word of the frame, including the
Java-locals and spill words of a frame published
`CompiledMethod::frame_holds_no_references`. The contract that flag carries
(`docs/internal/fixed-bugs/r11w14-rt-ir-lower-oop-free-frame-patch-FIXED-20260925.md`,
contract 1) is that no word below `frame_layout.callee_saved_lo` is ever a
reference. The band verifier, the map and the remap already trust it
(`band_has_unpublished_young_word`, `active_map_slots`,
`remap_one_jit_frame`); the root scan does not.

The edit: in `scan_one_frame_filtered`, `continue` for
`cm.frame_holds_no_references && !layout.callee_saved_shallow &&
layout.callee_saved_lo > 0 && off < layout.callee_saved_lo` before the word
is read, counted like the other exclusions (`note_excluded_band_word(...,
"reference-free")`), and route such a frame to the filtered arm in
`scan_one_compiled_frame_with_layout`. The register images at or beyond
`callee_saved_lo` (the CALLER's registers) stay scanned.

## What it buys

* No conservative root, and on G1 no pinned region, from a stale heap word an
  earlier frame left in a reference-free frame's locals or spills.
* Then the IR prologue's unset-locals, `shadow_savebase` and phi-scratch
  zeroing can go for such a body (step 4 of the page above): one store per
  slot per activation on call-heavy primitive code.
* A shorter band walk per such frame at every collection.

## Why it is a proposal and not a fix

It turns a contract the collector currently only uses to SKIP work
(verification, remap) into one it uses to DROP roots. If an IR body were
ever published reference-free while a word below `callee_saved_lo` held a
reference (a helper staging a reference argument into the frame, a future
`Op` whose result is a reference but not a `Ref` node), the object would be
freed or moved under the frame. That needs either a proof over every
emitter of `ir_lower.rs` or an opt-in flag (`CRATONVM_*`, default off) and a
soak on all three collectors. Retire it by landing it behind such a flag
with that soak, or by rejecting it.
