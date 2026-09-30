# `--compatible` `ConcurrentHashMap` has no tree bins: why a JDK-shaped treeify was declined, and what HT-3 costs

Status: OPEN (accepted `--compatible` divergence with its cost written down; the fix is proposal HT-3 for the CHM family, owner decision)
Area: `native-collections/src/lib.rs` (the native segmented `ConcurrentHashMap`, `register_concurrent_hashmap_natives`), `native-api/src/retired_shadow.rs` (the CHM retired rows), `vm/src/jit/helpers.rs` (`LeafNativeKind::ChmGet`)
Severity: LOW (call counts and the iteration order of a bin of 8+ colliding keys; no wrong answer)
Found by: round 13 wave 13 lane compat7

## What differs

HotSpot's `ConcurrentHashMap` turns a bin of 8 nodes in a table of 64+ slots into a `TreeBin`
(`treeifyBin`). The native CHM never does, so for colliding user keys:

* `R13HashcompatEqualsOrder --compatible`, line `ConcurrentHashMap colliding`, stays
  `put=66/66/12/0 get-eq=78/0/12/0 get-same=66/66/12/0 absent=48/0/4/0 overwrite=15/0/3/0
  remove=15/0/3/0` against HotSpot's `55/55/12/37 38/0/12/25 26/26/12/25 20/0/4/12 11/0/3/8
  7/0/3/4` (the last line of that probe off HotSpot);
* such a bin iterates in insertion order where `TreeBin.putTreeVal` PREPENDS each new node to
  `first` (`x = new TreeNode(h, k, v, f = first, null); first = x`);
* every operation on such a bin is O(n) `equals`.

## Why the wave-11 approach (run the JDK's own tree code) does not transfer

Wave 11 made `HashMap` / `LinkedHashMap` tree bins exact by calling the JDK's BIN-LOCAL tree
methods (`getTreeNode`, `putTreeVal`, `removeTreeNode`, `split`, `untreeify`, `treeifyBin`) on the
receiver's real `table`. For `ConcurrentHashMap` the equivalent pieces do not line up:

1. **No real table to run on.** The native CHM is 16 segments of `ClassId(0)` bucket maps
   (`CHM_FIELD_SEGMENTS`), not the JDK's `Node[] table`. `ConcurrentHashMap.treeifyBin(tab, index)`
   is an instance method that reads `tab.length` against `MIN_TREEIFY_CAPACITY` (the WHOLE
   table's length, which the natives only model as a number, `chm_note_table_high_water`), calls
   `tryPresize`, and publishes with `setTabAt` (Unsafe on the real table). None of that can be
   pointed at a segment's bucket array.
2. **The split is not a callable method.** `HashMap` resizes a tree bin with
   `TreeNode.split(map, tab, index, bit)`; CHM's `transfer()` splits a `TreeBin` INLINE (the
   `lo`/`hi` `TreeNode` lists, `untreeify` when a half is `<= UNTREEIFY_THRESHOLD`, a new
   `TreeBin` otherwise). A native segment resize (`map_resize_inner`) would have to re-implement
   that part, which is the from-scratch red-black bookkeeping wave 8 declined.
3. **A `TreeBin` head changes every reader.** The bin head would be a `ConcurrentHashMap$TreeBin`
   (hash `TREEBIN = -2`, nodes under `first`, a `lockState` reader/writer protocol that `find`
   runs with Unsafe CAS). Every native that walks a segment chain by `next` would have to learn
   it, lock-free readers included: `chm_seg_get_impl` (the snapshot walk),
   `chm_seg_get_wrapper_key_hashed` / `chm_wrapper_chain_walk` (the in-place walk),
   `native_chm_get_string_chain` and the per-thread String-node memo it feeds
   (`chm_string_node_cache_*`, `native-api/src/registry.rs`), the JIT's never-blocking door
   `jit_chm_get` (`LeafNativeKind::ChmGet`, `vm/src/jit/helpers.rs`), `chm_collect_all_entries` /
   `_keys` / `_values` and `chm_reorder_by_virtual_bucket` (iteration), `chm_seg_bin_holds_hash`,
   the compute family (`chm_seg_remap_single_walk`, `chm_compute_if_absent_single_walk`, the
   reservation protocol), the segment `put` / `remove` / resize through the `HashMap` writers
   (`native_map_put_evict_pinned_inner`, `native_map_remove_pinned_hashed`, `map_unlink_node`,
   `map_resize_inner`), `chm_publish_real_table_pinned`, and the 23 `KeySetView` natives
   (`register_chm_key_set_view_natives`). Wave 11's `HashMap` conversion touched a smaller set
   and still needed ~2000 lines; this one adds a concurrent reader protocol on top.
4. **Even then the order would not be exact.** Item 4 of
   `r13w8-hashcompat3-compatible-collection-residuals-20260928.md`: the native reconstructs CHM
   iteration order from a virtual table size and cannot reproduce `transfer`'s `lastRun` reversal;
   tree bins would add a second order the reconstruction does not model.

So a native `TreeBin` is not the compat6 shape; it is the rewrite the treeify page declined in
wave 8.

## The exact cost of HT-3 for the CHM family

HT-3 (`jit-r13-hashtree-proposals-RETIRED-20260929.md`): a per-VM `--compatible` sub-policy that lets the RETIRED
shadow triples of one family yield to the JDK's bytecode, as `--jdk-only` already does.

* **What exists already.** The CHM triples are retired rows in `native-api/src/retired_shadow.rs`
  (the `java/util/concurrent/ConcurrentHashMap` block from `<init>()V` on), so `--jdk-only` runs the
  real `ConcurrentHashMap` today and the default-mode probes (`R13HashcompatEqualsOrder`,
  `R13Compat6TreeBins` line `chm`, `R13Compat7Collections` line `chm-cia`) match HotSpot there.
  The mechanics are proven; only the policy is missing in `--compatible`.
* **The change, by file (none of it in `native-collections`, which is why this lane did not do
  it):**
  1. `VmConfig` (per VM, never a process global -- AGENTS.md): a set of families whose retired rows
     yield under `--compatible`, default empty, e.g. `CRATONVM_COMPAT_YIELD_FAMILIES=
     java/util/concurrent/ConcurrentHashMap` (opt-in until measured, then default-on per family
     with a kill switch; `--compatible` must not drift unasked).
  2. The yield decision (`resolve_dispatch` / the retired-shadow consult in `native-api` and its
     VM caller, plus `jit_direct_helper_refused` so the JIT stops binding the native direct): one
     extra predicate, `retired && (jdk_only || family_yields(config))`.
  3. `LeafNativeKind::ChmGet` (`vm/src/jit/helpers.rs`): the site resolves to the bytecode `get`
     once the triple yields, so the leaf door simply stops being selected; nothing to delete for
     the arm, dead code to remove once the family is default-on.
  4. Natives that ALLOCATE a CHM themselves and hand it to Java must stop doing so for a yielding
     family, or the object reaches bytecode without a `table`: `native_chm_init*` are the
     constructors (they yield with the family), but the `Properties` natives keep a side CHM of
     their own (`Properties` is `CF_HASHTABLE_ANCESTRY`, not in the family) and must be checked to
     never expose it; `native_chm_init_from_map` and `chm_init_segments` callers outside the
     constructors need the same audit (a grep for `chm_init_segments`).
  5. Measurement, which HT-3 names as the gate: `CratonBench` CHM-heavy kernels and a
     `ConcurrentHashMap` stress probe in `--compatible` with the family armed vs not (the native
     `get` is a Rust walk with a JIT leaf door; the bytecode `get` is JIT-compiled Java with
     Unsafe volatile reads -- the speed is unknown in either direction), plus the WildFly
     `parallel-extension-add` boot (the CHM deadlock history is entirely in the NATIVE protocol,
     so arming the family removes it rather than adding to it).
* **What it closes.** The `ConcurrentHashMap colliding` line of `R13HashcompatEqualsOrder`; CHM
  iteration order after a resize (hashcompat3 item 4); `TreeBin` order and O(log n) behaviour for
  colliding keys; the reservation protocol's residual deviation (a racing `put` during a
  `computeIfAbsent` mapper); and, when default-on, the ~6 000 lines of `chm_*` natives with it (functions named `*chm*` in `native-collections/src/lib.rs`).
* **Estimate.** Items 1-3: one small change in `native-api` + the VM (a day, including the
  per-VM plumbing test). Item 4: an audit, likely one or two call sites. Item 5: the long pole --
  one measurement wave on the Windows battery and the Linux Spring/WildFly hosts.

## How to confirm the divergence (and a fix)

* `R13HashcompatEqualsOrder --compatible`, line `ConcurrentHashMap colliding` (differs today;
  HotSpot's with the family armed).
* `R13Compat7Collections --compatible` line `chm-cia` must stay HotSpot's either way (it does not
  reach a tree: three colliding keys).
