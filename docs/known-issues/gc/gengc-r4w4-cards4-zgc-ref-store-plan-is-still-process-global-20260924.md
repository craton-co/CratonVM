# ZGC's compiled-reference-store plan is still the process-global block, so two ZGC heaps in one process share one post gate

*Filed 2026-09-24 by generational GC round 4, wave 4, lane `cards4`.*

- **Status:** open. Latent: it needs two ZGC heaps in one process.
- **Severity:** correctness, latent. The same shape as the generational
  defect this lane fixed
  (`docs/internal/gc/gengc-r4-cards-jit-post-barrier-gate-is-process-global-FIXED-20260924.md`),
  now confined to ZGC-vs-ZGC.
- **Code:**
  - `gc/src/gen_heap.rs`: `JIT_REF_STORE_GATES`, `publish_jit_ref_store_plan`,
    `set_jit_ref_store_post_active`, `clear_jit_ref_store_plan`
  - `gc/src/zgc.rs`: `ZgcRealHeap` construction (`publish_jit_ref_store_plan`),
    `Drop` (`clear_jit_ref_store_plan`), `reset_generational_state`
    (`set_jit_ref_store_post_active(false)`), the generational sweep arm that
    re-arms it (`set_jit_ref_store_post_active(true)`)
  - `vm/src/jit/helpers.rs::build_helpers_opt`: the non-generational fallback
    to `jit_ref_store_gate_addrs()` / `jit_ref_store_post_skip_mask()`

## What is left

Wave 4 moved the GENERATIONAL post plan into a per-heap block
(`GenerationalHeap::jit_ref_store_plan`). ZGC still publishes into, and clears,
the process block. With two ZGC heaps in one process — one running
generational ZGC (`CRATONVM_ZGC_GENERATIONAL`, old objects exist, `post_active`
must be 1) and one that calls `reset_generational_state` — the second heap's
`set_jit_ref_store_post_active(false)` makes the first heap's compiled
reference stores skip ZGC's post barrier. That is a lost remembered-set entry
for an old-to-young edge.

A heap `Drop` (`clear_jit_ref_store_plan`) is conservative (it ARMS both
gates), so teardown is not a hazard, only `reset_generational_state` is.

## Why it was not fixed here

`zgc.rs` belongs to the ZGC owner; this lane owned only the
`reset_generational_state` hunk, and a per-heap block needs a field on
`ZgcRealHeap`, its construction, `Drop`, and the sweep arm. Changing only the
reset to a counted decrement, without the matching increment at the arming
site, would make the count go negative-then-saturate and read "armed" forever
(safe, but it would silently withdraw ZGC's gated fast path).

## Proposed fix (S)

Mirror the generational change: `ZgcRealHeap` owns a
`Box<JitRefStoreGates>`, publishes its FLOOR plan into it at construction,
writes its own `post_active` in `reset_generational_state` and the sweep arm,
and exposes `jit_ref_store_plan()`. `build_helpers_opt` then reads
`VmHeap::Zgc(h) => h.jit_ref_store_plan()`, and the process block is left with
only the pre byte (and the VM-less unit-build fallback).

**First step:** a unit test in `gc/src/zgc.rs` that builds two
`ZgcRealHeap`s, enables generational mode on one, resets the other, and
asserts the first heap's post byte (through whatever `build_helpers_opt` would
hand compiled code) is still 1. It fails today.
