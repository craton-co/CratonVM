# G1's final remark may unregister the layouts of classes whose dead instances it leaves in place

*Filed 2026-09-26 by gen round 5 wave 3, lane `unload7`. SUSPECTED defect,
not reproduced; G1 is out of scope for this round, so nothing was changed.
Severity if real: medium (a heap walk that breaks forever at one object).*

## What may be wrong

`g1_remark_process_references` (`vm/src/runtime/interpreter/gc_and_alloc.rs`)
runs the class-unload transaction (`unload_dead_class_metadata`) at the final
remark. `ClassStore::remove` unregisters each unloaded class's compact field
layout (`cratonvm_types::unregister_class_layout`), and a compact object's
size is derived from that layout: `cratonvm_types::object_instance_size`
answers `IMPLAUSIBLE_BODY_SIZE` once it is gone.

G1's cleanup then frees only the WHOLLY dead regions. A dead compact instance
of an unloaded class in a region that also holds live objects stays in place
until that region is evacuated by a later mixed collection. Any linear walk of
such a region in between — a remembered-set / card scan that walks objects from
a card boundary, a region verifier, a heap dump — must size that object and
cannot.

The generational concurrent cycle has exactly this shape (its sweep frees the
dead instances after the remark), which is how it was found there; the fix on
that side is `memory::gc::with_retained_unloaded_layouts`
(`gengc-r5w1-refs5-concurrent-cycle-cannot-unload-classes-20260926.md`,
hazard 3).

## Evidence

Reading only: `g1.rs` was not opened for this (out of scope). Whether G1's
region walks size dead objects through `object_instance_size` (or through
`zgc.rs`-style guarded sizing, which refuses them) decides whether this is
real. ZGC documents the same refusal for its registry (`zgc.rs`, "a class
unloaded by `ClassStore::remove` -> `unregister_class_layout` while an instance
is still in this heap's registry").

## Proposed fix

If a G1 walk can meet such an object: run G1's remark transaction under
`memory::gc::with_retained_unloaded_layouts` as the generational remark now
does (one line), and share the release rule of
`gengc-r5w3-unload7-retained-layouts-are-never-released-20260926.md`.

## How to verify

`-XX:+UseG1GC` with the `RClassUnloadSweep` payload made compact and given many
instances interleaved with long-lived objects in the same regions, a final
remark that unloads it, then a young collection with dirty cards over those
regions: no `old-gen scan_region: BREAK` / G1 walk refusal, and a later mixed
collection frees the dead instances.
