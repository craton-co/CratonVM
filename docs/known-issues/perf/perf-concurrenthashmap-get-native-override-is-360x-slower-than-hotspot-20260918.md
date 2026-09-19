# `ConcurrentHashMap.get` in a hot loop is ~360x slower than HotSpot (native override, not the JIT)

**Status:** PARTIALLY FIXED (round 9 wave 11, chm11) -- the native side (lock-free,
allocation-free read path for primitive-wrapper keys, single hashing, JIT leaf probe
`jit_chm_get`) is landed; wiring the probe into the JIT site-cached leaf dispatch is an exact
cross-lane edit to `../../../vm/src/jit/helpers.rs` (see "Status after wave 11"). Filed by the integrator
from lane `irl10`'s side finding (round 9 wave 10, `../../internal/jit-review-r9/NOTES-w10-irl10.md`).
**Owner-area:** the VM's `ConcurrentHashMap` native override (`native-builtins` /
`native-collections`), and whatever makes the JIT dispatch to it (`dispatch_static.rs`,
`native_override.rs`).

## Evidence

A hot loop of `ConcurrentHashMap.get` measured 34.5 s on `cratonvm-jitr9-w9b.exe` against
96 ms on HotSpot 25 (lane irl10, 2026-09-19). CratonVM replaces `ConcurrentHashMap.get` with a
native, so every compiled call pays a generic native dispatch and the native's own locking,
where HotSpot inlines a lock-free read of the bin array.

## Direction

1. Measure first (interleaved vs HotSpot and vs `HashMap.get`, which the JIT now handles
   through the ArrayList/collection work of waves 3-10): how much is the generic native call
   (see `perf-collection-natives-cost-a-generic-native-dispatch-per-call`), how much the
   native's lock.
2. Either route `get` through the real JDK bytecode (the class bytes are authoritative per
   AGENTS.md; the override may only exist for bootstrap reasons) so the JIT can inline it, or
   give the native a lock-free read path and a JIT leaf-call entry like the collection natives.

## Status after wave 11 (chm11)

### Measured first (w10 binary, `CRATONVM_JIT_RETIRE_CELL=1`, 2026-09-19, this box)

`VolSplice chm` (30 M `get(Integer)`, 1024-entry map): **35 378 ms** vs HotSpot **123 ms**
(~1.18 us per lookup). Sampler (`diag/sampler.py 4 4`, 3 465 busy samples) -- where it goes:

* **~75-80 % inside `native_chm_get` itself**: `is_object_address` 10 %, `compact_field_slot`
  7.2 %, zgc `get_field` 6.9 %, `get_field_volatile_as` 4.1 %, `read_compact_field` 3.6 %,
  `resolve_field_descriptor_byte_cached` 2.8 %, `chm_seg_get` 2.7 %, `load_and_forward_inner`
  2.5 %, `VersionCache::find` 1.9 %, unbox (`fast_unbox_primitive_wrapper` + `unbox_wrapper`)
  3.1 %, pins 2.8 %, `is_real_java_string` 1.2 % (the String fast path probing an Integer).
  Cause: an `Integer` key had NO fast path. The String path scanned its memo and declined, then
  the general path paid `chm_key_hash`, four pins, a SECOND `map_hash_key` inside
  `chm_seg_get`, the stripe read lock, two heap `Vec`s for the chain snapshot, seven
  descriptor-resolving checked `get_field`s per chain node (`get_node_hash`/`_key`/`_value`
  each re-read slot 0 to sniff the layout), per-node pins, and two unboxes in
  `map_keys_equal`.
* **~13 % the generic native dispatch** (`safe_native_call_impl` 5.5 %,
  `try_jit_site_cached_native_dispatch` 3.9 %, `forward_jit_reference_args` 1.8 %,
  `decode_dispatch_values_into_shaped` 1.0 %, `jit_invoke_virtual_mic_body` 0.9 %).
* ~5 % the loop's own `Integer.valueOf(i & 1023)` boxing (keys above the cache), not this page.
* The native's LOCK was not the cost: an uncontended `parking_lot` read lock is a few ns.

"Route `get` through the real JDK bytecode" is not available: CratonVM keeps a CHM's entries in
its Rust-managed segment layout and the real `table` field stays null, so the bytecode would
answer null for every key.

### Done (native side, `../../../native-collections/src/lib.rs`)

* **`chm_get_wrapper_key_fast`** (+ `chm_wrapper_key_hash`, `chm_seg_get_wrapper_key`,
  `chm_wrapper_chain_walk`): for a JDK primitive-wrapper key (all `final`, value-equal, no Java)
  the chain is walked IN PLACE -- no pins, no snapshot `Vec`s, one hash, one layout sniff per
  node, nodes of the memoized real `HashMap$Node` class read with ONE `get_fields_typed` (no
  descriptor lookup, no checked membership walk per slot), the value read only on a hit.
  Concurrency: the String path's protocol -- optimistic walk validated by the stripe resize epoch
  (seqlock, with an acquire fence before the re-check), redone under the stripe read lock if it is
  FREE, otherwise declined to the general path. It never blocks (see below). Equality is
  `map_keys_equal`'s exactly (identity, else same wrapper class + the shared
  `wrapper_prims_equal`, split out of `map_keys_equal` so both use the same arms); reservation
  markers read as absent; anything unexpected declines. `native_chm_get` tries it first.
* **`chm_seg_get` takes the caller's hash** (all five callers: `get`, `containsKey`,
  `getOrDefault`, `computeIfAbsent`'s present-key probe, `computeIfPresent`'s absent-key probe).
  It hashed the key a second time -- for a user key that ran its Java `hashCode()` TWICE per
  lookup, where the JDK runs it once. Wrapper keys take the in-place walk there too. Its two
  early `return Ok(None)`s after pinning now unpin.
* **`pub fn jit_chm_get(ctx, this, key) -> Option<Value>`**: the no-allocation / no-Java /
  no-throw / never-blocking leaf probe (wrapper keys, then String keys via
  `native_chm_get_string_fast`), same contract as `jit_overlay_hashmap_get`/`jit_arraylist_get`.
  `native_chm_get_string_fast`'s locked retry is now `try_read` for the same reason.
* Tests (`../../../native-collections/src/lib.rs`, `mod tests`):
  `r9w11_chm_wrapper_get_answers_like_the_general_path`,
  `r9w11_chm_wrapper_get_typed_node_route_answers_the_same`,
  `r9w11_chm_wrapper_get_survives_a_concurrent_in_place_relink` (a writer thread repeatedly cuts
  a chain in place under `ChmSegmentResizeGuard` while a reader looks up every key of it: never
  absent, never wrong; declines are answered by the native).

### Remaining

1. **JIT leaf wiring** (`../../../vm/src/jit/helpers.rs`, not this lane's file): `LeafNativeKind::ChmGet`
   classified on `native_chm_get` identity and served by `jit_chm_get`, plus the same probe in
   front of the funnel in `jit_concurrent_hashmap_get_direct_body`. Exact edits in
   `../../internal/jit-review-r9/NOTES-w11-chm11.md`, "Cross-lane requests". That removes the
   ~13 % funnel.
2. The loop's `Integer.valueOf` boxing (JIT/boxing lanes).
3. Expected after (native side only, not measured -- the w10 binary predates it): the per-get
   native cost drops from ~0.9 us to roughly a dozen heap accesses (~0.1-0.2 us); with (1) the
   whole call should land near the ArrayList leaf's cost. Re-measure with
   `VolSplice chm` interleaved against `cratonvm-jitr9-w10.exe`.

## Integrator measurement after wave 11 (w11 binary, 2026-09-19)

The cross-lane `helpers.rs` edits (`LeafNativeKind::ChmGet`, and the `jit_chm_get` probe in
`jit_concurrent_hashmap_get_direct_body`) are applied. `VolSplice chm` (30 M gets), interleaved:
w10 32 546 / 32 356 ms, w11 17 600 / 17 677 ms, with the same checksum. HotSpot takes about
100 ms, so the page stays open. What is left is the call itself: the generic native dispatch,
plus the loop's `Integer` boxing.
