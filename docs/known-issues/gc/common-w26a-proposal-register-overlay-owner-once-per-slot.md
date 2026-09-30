# common-w26a proposal: register an overlay owner once per slot, not on every overlay operation

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 22
> of 54).** Not built (`widened_obj_key` still ends in
> `register_overlay_owner_key`). **Gate:** `MtChurnProbe` / a
> TreeMap-LinkedHashMap churn A/B with the opt-in on, plus the debug drift
> assertion clean in the test suite. **Size:** S.

- **Status:** PROPOSAL (not a defect). Filed 2026-09-25, gc-common round
  2026-09-23, wave 26, lane A26.
- **Owner:** `native-collections/src/lib.rs` (`widened_obj_key`,
  `register_overlay_owner_key`).

## What happens now

Every `widened_obj_key` call -- every TreeMap / TreeSet / LinkedHashMap /
LinkedList / CSLM / snapshot-iterator overlay operation, on every thread --
ends in `register_overlay_owner_key(ptr, key, class)`, which takes the ONE
process-global `overlay_owner_keys()` mutex and scans the owner's key `Vec`
for `key`. On a steady-state hit (step 1: the slot already records this
address) the insert is a no-op: the row was written when the slot was
minted and has been kept in step since. The registry itself was sharded
64 ways precisely because one global mutex per overlay op was "the dominant
contention point" (its PERF comment); the owner index put one back.

Wave 26 removed the two cheaper-to-fix parts of this cost (the unconditional
`fetch_or` on the class bitmap, and holding the registry shard across the
owner-index insert) but not the mutex.

## Proposal

Register the owner only when `widened_obj_key` MINTS a slot (step 2), and
skip it on a step-1 hit. It is sound if the owner index and the slot
registry stay in lockstep, which after w26-a they do in production:

- a row `(addr, key)` is created only with its slot (`last_ptr == addr`);
- it is removed only with its slot: `drop_overlay_entries_for_dead_keys`
  (from the prune and from `forget_vm_collection_overlays`, both of which
  drop the slot first) -- the only other remover,
  `clear_overlay_entries_for_key`, is test-only since w26-a;
- it is moved only by `update_collection_overlay_refs`, with the same
  pointer map, in the same call, as the slot's `last_ptr` (a key proven
  foreign keeps its address, and so does that VM's slot).

What would have to be true, and checked, before turning it on: no future
path removes or moves an owner row without its slot. A debug assertion (on
a step-1 hit, `overlay_owner_still_at(ptr, key)`) under
`cfg(debug_assertions)` would catch a drift in tests. The cost of getting
it wrong is severe -- a missing owner row makes the per-owner markers treat
a live collection's backing as unreachable -- so it should ship behind a
`CRATONVM_*` flag defaulting OFF first, with an A/B on a collection-heavy
multi-threaded probe (`MtChurnProbe`, a TreeMap/LinkedHashMap churn).

## Confirmation grep

```
rg -n "register_overlay_owner_key\(ptr, key" native-collections/src/lib.rs
```
