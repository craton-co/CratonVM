# `--compatible` hash containers: what still differs from the JDK after round 13 wave 8

Status: OPEN (round 13 wave 9 fixed items 1, 2, 6, 7 and the `HashMap` / `LinkedHashMap` half of item 5; wave 10 fixed the `ConcurrentHashMap` hash count of item 5, the `TreeMap` value-`equals` order of item 8 and the overwrite hole of item 3, pending orchestrator verification; wave 11 fixed the CHM `compute` / `computeIfPresent` / `merge` `equals` counts of item 5; wave 13 fixed the CHM `computeIfAbsent` walks, the last of item 5; left: reversed-view writes and `LinkedHashSet.reversed()` (item 3, proposal C5-1), item 4 (CHM order after a resize, HT-3), `TreeMap` walk counts (item 8, not fixable on the native array, HT-3), see "Round 13 wave 13"; round 14 wave 3 made `LinkedHashSet.reversed()` the JDK's live view (CP2-2), leaving removal through its iterator and `LinkedHashMap.reversed()`; round 14 wave 4 made the reversed key / entry sets live (C3-1) and `LinkedHashMap.reversed()` the JDK view (C3-2), leaving the reversed `values()`; round 14 wave 5 made the reversed `values()` a live `LinkedValues(true)` (C4-1), which closes item 3 pending orchestrator verification; items 4 and 8 wait on the HT-3 owner decision)
Area: `native-collections/src/lib.rs` (the native `HashMap` / `LinkedHashMap` / `LinkedHashSet` / `HashSet` / `ConcurrentHashMap` family, live only under `--compatible`)
Severity: LOW-MEDIUM (items 1-4 are observable by ordinary programs; items 5-8 are call counts of `hashCode()` / `equals()` that only a side-effecting key sees)
Found by: round 13 wave 8 lane hashcompat3

Wave 8 fixed what the five `--compatible` probe diffs pointed at (see the wave-8 sections of
`r13w4-hashcompat-compatible-maps-diverge-from-jdk-comparison-order-FIXED-20260928.md` and
`r13w4-hashcompat-native-hashmap-never-treeifies-FIXED-20260929.md`). Reading the neighbouring natives
against the JDK bodies found the items below. None of them is reachable under the default
`--jdk-only`, where real `java.util` bytecode serves these classes.

## Observable by ordinary programs

1. **`HashMap.forEach` hands the action snapshot values.** `native_map_for_each` collects
   `(key, value)` pairs up front (`collect_entries_any`) and then calls `accept`. The JDK walks the
   live table (`action.accept(e.key, e.value)`), so a value that the action replaces for a key it
   has not reached yet arrives NEW on HotSpot and OLD here. The wave-8 change only added the
   after-the-loop `ConcurrentModificationException`. Fix: for the receivers
   `map_replace_all_by_nodes` accepts, collect the NODES (`hs_backing_nodes`) and read
   `get_node_key` / `get_node_value` at visit time, exactly as `map_replace_all_by_nodes` does.
   Confirm: `m.forEach((k, v) -> { if (k.equals("a")) m.put("b", 99); seen.add(v); })` on a
   `HashMap{a=1, b=2}`: HotSpot `seen=[1, 99]`.
2. **Access-ordered `LinkedHashMap.compute` / `computeIfPresent` / `merge` move the entry BEFORE
   the function runs.** These route through `native_map_compute*` / `native_map_merge`, which open
   with `native_map_get`, i.e. `native_lhm_get`, which moves the entry to the tail. The JDK looks
   the node up with `getNode` (no access) and calls `afterNodeAccess` only after a non-null result.
   A function that throws, returns null, or looks at the map's order sees a different order.
   Fix: an LHM arm in the three natives that finds the node with `lhm_find_node_hashed` (one hash)
   and calls `lhm_move_to_tail` after a non-null result; it would also remove the second
   `hashCode()` (item 5).
3. **`LinkedHashMap.reversed()` / `LinkedHashSet.reversed()` are snapshots** (recorded in the code
   at `native_lhs_reversed` and `build_reversed_map_snapshot`): a write to the source after the view
   was taken is not seen until the next resync (`LinkedHashMap`) or ever (`LinkedHashSet`).
   The JDK returns live views (`ReversedLinkedHashMapView`, `ReverseLinkedHashSetView`).
4. **`ConcurrentHashMap` iteration order is reconstructed, not kept.** `chm_reorder_by_virtual_bucket`
   sorts by `hash & (table - 1)` over the JDK table size (now including the high-water mark and the
   `putAll` presize, wave 8). Two keys in one virtual bucket keep the native chain order, while the
   JDK's `transfer` splits a bin with its `lastRun` shortcut, which REVERSES the part of a bin before
   `lastRun`. A map whose bin held keys that went to the high half before a same-bit run can
   iterate in a different order after a resize. A CHM that was emptied by `remove` and then
   `putAll`-ed is still presized by the JDK (its table exists) and not here (the count is 0).
   Also `putAll` into a CHM whose table exists presizes only through `native_chm_put_all`, not
   through the other bulk producers. Fix, if it is ever worth it: keep a per-map record of the JDK
   table (not only its size) or retire the native CHM under `--compatible`
   (`jit-r13-hashtree-proposals-RETIRED-20260929.md` HT-3).

## Call counts only a side-effecting key sees

5. **The compute family still hashes twice.** `HashMap` `computeIfAbsent`, `compute`,
   `computeIfPresent`, `merge` (`native_map_compute*`, `native_map_merge`) are `get` + callback +
   `put`/`remove`: two `hashCode()` calls and two chain walks where `HashMap` makes one. The
   `ConcurrentHashMap` twins (`native_chm_compute*`, `native_chm_merge`, and the reservation protocol
   in `chm_compute_if_absent_absent_path`: `get`, `put` marker, `get`, `put` value) make up to five.
   Fix for the `HashMap` family: hash once with `map_hash_key`, find the node with
   `map_chain_find_from`, run the callback, then (the modCount check having proved the table
   unchanged) write the node or insert with `native_map_put_evict_pinned_hashed(.., Some(hash),
   ..)`. For CHM, a raw hashed segment lookup (the reservation-aware `native_map_get` without its
   own hash) is needed first.
6. **`Iterator.remove()` on a native key/entry snapshot iterator hashes the key.**
   `native_map_key_itr_remove` removes by KEY (`native_hs_remove`), because the iterator holds a key
   snapshot, not nodes. The JDK removes the node (`removeNode(node.hash, ..)`), no `hashCode()`.
   This is the one extra call left in `R13ShadowHashSetFamily` scenario 5 (`9` against HotSpot's
   `8`) and the reason its `sig` line still differs. Fix: carry nodes (or node hashes) in the
   snapshot, or, for a `LinkedHashMap` backing, look the node up by identity from the head with a
   small bound before falling back.
7. **`LinkedHashMap.reversed()`'s rebuild re-hashes every entry** (`rebuild_reversed_from` puts each
   entry through `native_lhm_put`). `LinkedHashSet.reversed()` no longer does (wave 8,
   `lhm_append_distinct`); the same helper, fed the source nodes' hashes, would serve the map.
8. **`native_map_remove_kv` / `native_map_replace*` on `Properties`, `TreeMap`, `IdentityHashMap`
   and the unmodifiable wrappers** keep the composite (`get` + `put`/`remove`). Only `TreeMap`'s is
   JDK-visible (its `compareTo` count), and TreeMap is outside this lane.

## How to confirm

* Items 1-2: `C:\craton\jitr13-probes\src\R13Hashcompat3Semantics.java` covers the exceptions; add
  the `seen` line of item 1 when fixing it.
* Item 6: `R13ShadowHashSetFamily --compatible`, `hashCode calls [31, 31, 31, 31, 68, 9]`.
* Item 5: `R13Hashcompat3Order` has no compute line on purpose; add one (a colliding chain of five,
  `computeIfAbsent` of an absent key: HotSpot `1/4`).

## Round 13 wave 9 (lane hashcompat4)

Every change is default ON in both modes behind its own switch (`=0` restores the old body; all
read through `compat_switch_on`). None of them is reachable under `--jdk-only`, where real
`java.util` bytecode serves these classes.

### Landed

* **Item 1, `HashMap.forEach` snapshot values** -- `CRATONVM_COMPAT_MAP_FOREACH_LIVE`
  (`map_for_each_live`, `hm_int_fast_lookup`). A plain `HashMap` (no integer-overlay row) walks
  its own table as the JDK body does: `tab` captured once, each chain followed through `next`
  after the action ran, so a value the action replaced arrives new and a node it unlinked is not
  visited; the after-the-loop `ConcurrentModificationException` is unchanged. An overlay map
  (`Integer` keys) keeps the key snapshot but reads each value from the overlay when it is handed
  over, and skips a key the action removed.
* **Items 2 and 5 (`HashMap` / `LinkedHashMap`)** -- `CRATONVM_COMPAT_MAP_COMPUTE_JDK`
  (`map_remap_route`, `map_remap_jdk`). `computeIfAbsent`, `compute`, `computeIfPresent` and
  `merge` on a plain `HashMap` (non-wrapper key, no overlay row) and on every `LinkedHashMap`
  (`native_lhm_compute_if_absent` too) now run the JDK bodies' shape:
  * one `hash(key)` and one chain walk before the function; on the removal path the JDK's second
    walk (`removeNode(hash, key, ..)`) and, for `computeIfPresent` only, its second `hash(key)`;
  * **new finding, fixed with it:** a NEW key is linked at its bucket's HEAD
    (`tab[i] = newNode(hash, key, v, first)`), where `put` appends. This is iteration order with
    ordinary `String` keys: `m.put("Aa", 1); m.computeIfAbsent("BB", ..)` iterates `[BB, Aa]` on
    HotSpot and iterated `[Aa, BB]` here (the grouping idiom `computeIfAbsent(w, k -> new
    ArrayList<>())` hits it whenever two keys share a bucket). `LinkedHashMap` relinks the new
    node to its bucket head too (`lhm_relink_bucket_head`; it iterates its list, so only the
    `equals` order of later lookups sees that);
  * **new finding, fixed with it:** the table grows on ENTRY only (`size > threshold ||
    table == null`), never after the insert. The composite's `put` grew first, so the 13th key
    added by `computeIfAbsent` doubled a 16-bucket map where HotSpot keeps 16 buckets until the
    next insert (probe line `no-grow`);
  * **new finding, fixed with it:** a key mapped to `null` is present. `compute(k, f)` returning
    null now removes it (the composite's `get` saw "absent" and left the mapping);
    `computeIfAbsent` writes into the existing node;
  * access-ordered `LinkedHashMap`: `afterNodeAccess` runs only after a non-null result (a
    throwing function, or one that reads the order, sees the entry where it was);
  * the `modCount` check around the function now also covers `LinkedHashMap.computeIfAbsent`,
    which the old native did not check. A receiver without a readable `modCount` compares size
    and table length instead and, when those moved, falls back to the composite's by-key write
    (`remap_write_by_key`), as it also does under `CRATONVM_COMPAT_MAP_REMAP_CME=0`.
* **Item 6, `Iterator.remove()` hashes the key** -- `CRATONVM_COMPAT_ITR_REMOVE_BY_NODE`
  (`itr_snapshot_nodes`, `key_itr_remove_node`, `map_remove_node_by_walk`). An ordinary
  `HashSet` / `LinkedHashSet` iterator carries each element's node in the upper half of its
  snapshot array (the `MAP_KEY_ITR_FIELD_KEYS` array is `2 * total` long; `next`/`hasNext` read
  only the lower half). `remove()` now checks `modCount` first (the JDK's
  `ConcurrentModificationException` after its `IllegalStateException` test, which the native
  never made), then unlinks the node as `removeNode(p.hash, p.key, ..)` does: no `hashCode()`,
  and `p.key.equals(..)` for each same-hash node AHEAD of `p` in its chain (removing the third of
  three colliding keys: HotSpot `0/2`). The same walk now serves `retainAll`/`removeAll`'s
  iterator arm (`hs_filter_by_contains`, wave 8, which unlinked by identity and so skipped those
  `equals`), and `removeIf` on a `keySet()` view of a plain `HashMap`/`LinkedHashMap`
  (`hs_remove_if_by_nodes`: the source map's nodes, with the `next()`/`remove()` `modCount`
  checks). A plain `HashSet.removeIf` is `Collection.removeIf`'s bytecode over this iterator.
* **Item 7, `LinkedHashMap.reversed()` re-hashes** -- `CRATONVM_COMPAT_LHM_REVERSED_BY_NODE`
  (`rebuild_reversed_from`): a `LinkedHashMap` source fills the view from its nodes' stored
  hashes (`lhm_collect_hashes`, `lhm_append_distinct`), no `hashCode()` or `equals()`, on every
  rebuild.

### Found while reviewing (task item 3) and fixed

* **`Map.Entry.setValue` write-through** -- `CRATONVM_COMPAT_ENTRY_SET_VALUE_IN_PLACE`
  (`entry_set_value_in_place`). A live entry of a plain `HashMap` / `LinkedHashMap` wrote
  through with `put(key, v)`: an `equals` walk, a move to the tail of an ACCESS-ORDERED map (and
  with it a `modCount` bump, so `for (e : lhm.entrySet()) e.setValue(..)` threw
  `ConcurrentModificationException` from the next `next()`; HotSpot's `Node.setValue` neither
  reorders nor bumps), and a removed key PUT BACK. It now finds the node holding the very key
  object (by the key's hash, compared by identity) and writes its value; no node, no write. One
  `hashCode()` remains (see the views page below).
* **`HashSet(int)` / `HashSet(int, float)`** -- `CRATONVM_COMPAT_HASHSET_CTOR_JDK`
  (`native_hs_init_capacity`, `native_hs_init_capacity_load`): the backing is `new
  HashMap<>(initialCapacity[, loadFactor])` with the constructor's own arguments.
  `new HashSet<>(0)` started at 16 buckets (HotSpot: 1, then 2, 4, ...: `{1, 4}` iterates
  `[4, 1]` there and iterated `[1, 4]` here), and the factor was dropped (comparison-order page,
  wave-6 "still open" item 2).
* **`Map.equals` value test** -- `CRATONVM_COMPAT_MAP_EQUALS_JDK` (`map_equals_entries`).
  `AbstractMap.equals` / `Hashtable.equals` decide with `value.equals(m.get(key))`: the stored
  value's `equals`, also for the very same object and for a null answer. The native short-cut
  `==` and never asked about null. `Properties` (whose `equals` is its CHM's: `v.equals(val)` of
  the OTHER map) keeps the old test.

### Reviewed, not a defect

* `LinkedHashMap.removeEldestEntry` after `put` / `putIfAbsent` / compute inserts: consulted with
  the live head node, only for a subclass, only on an insert (`native_lhm_put_evict_hashed`), and
  the eviction is `removeNode(hash(key), ..)` by key, which is what the JDK does too.
* `HashMap.clone()`: no native; the real `clone` -> `reinitialize` -> `putMapEntries` -> `putVal`
  bytecode runs over the native table (which has the real layout), so the clone is built as on
  HotSpot, `putMapEntries`' presize included.
* `LinkedHashMap.containsValue` (comparison-order page, wave-6 item 3):
  `native_lhm_contains_value` is registered on `LinkedHashMap` and already walks the
  insertion-order list with the argument's `equals` (`list_element_matches(val, target)` asks
  `target.equals(val)`); that claim is stale.

### Still open

* **Item 3, reversed views are snapshots.** Unchanged, and one more hole found: the rebuild is
  keyed on the source's SIZE (`lhm_source_generation`), so a value OVERWRITE in the source
  (`m.put(existing, v2)`, `replace`, `Entry.setValue`) is never seen by a `reversed()` view taken
  earlier (HotSpot's view reads the node). A size + `modCount` generation would still miss
  overwrites (neither moves). Fix: a view carrier whose natives read through to the source in
  reverse, or a rebuild on every read (O(n) per read). `LinkedHashSet.reversed()` is still never
  rebuilt.
* **Item 4, CHM order after a resize**: unchanged.
* **Item 5, `ConcurrentHashMap`**: `native_chm_compute*` / `native_chm_merge` and the
  reservation protocol still hash 2-5 times: unchanged.
* **Item 8**: unchanged (`TreeMap` is outside this lane).
* New: `r13w9-hashcompat4-map-views-call-hashcode-FIXED-20260928.md` (building or resyncing a
  `keySet()` view `put`s every key, a `hashCode()` each; view removals go by key) and
  `r13w9-hashcompat4-overlay-remap-order-FIXED-20260928.md` (`Integer`-keyed `HashMap`s keep the
  composite: compute-family inserts append and grow first there).

### How to confirm (wave 9)

* `cargo test -p cratonvm-native-collections --test r13_hashcompat4_jdk_shapes` (9 tests, each
  fails on the old code), plus `r13_hashcompat3_jdk_counts`, `mock_hashmap`,
  `mock_lhm_access_order`, `map_conditional_mutators`, `r13_hashtree_single_walk`, `gc_*` and
  `abstract_collection_interception`.
* Probes `C:\craton\jitr13-probes\src\R13Hashcompat4Remap.java` and `R13Hashcompat4Nodes.java`
  (expected HotSpot lines in their headers; `bad 0`, `drift 0`) under `--compatible`, the default
  mode, and each switch at `0`; `R13ShadowHashSetFamily --compatible` scenario 5 should reach
  HotSpot's `8` `hashCode()` calls, and `R13Hashcompat3Semantics` must not move.

## Round 13 wave 10 (lane compat5)

Each change default ON in both modes behind its own `compat_switch_on` switch (`=0` restores the
wave-9 body); none reachable under `--jdk-only`. `native-collections/src/lib.rs`.

### Landed

* **Item 3, the wave-9 hole (value overwrites never seen by a reversed view)** --
  `CRATONVM_COMPAT_LHM_REVERSED_SEES_OVERWRITES`. `rebuild_reversed_from` now also records the
  `(key, value)` identities the view was built from (an `Object[]` under the overlay key
  `__reversed_fingerprint`, beside `__reversed_source`), and `resync_reversed_map` rebuilds when
  the source's SIZE moved OR those identities differ (`reversed_fingerprint_matches`: an O(n) walk
  of pure field reads, paid only by a reversed view). So a value overwrite (`put` of a present
  key, `replace`, `Entry.setValue`, a compute update) and a balanced `remove` + `put` in a
  `LinkedHashMap` source now show through: `m = {a=1, b=2}; r = m.reversed(); m.put("a", 9);`
  prints `{b=2, a=9}` (was `{b=2, a=1}`). A write made through the VIEW is left alone while the
  source is unchanged, as before (it still does not reach the source -- see below).
* **Item 5, the `ConcurrentHashMap` half** -- `CRATONVM_COMPAT_CHM_REMAP_SINGLE_HASH`.
  `computeIfAbsent`'s reservation protocol reads and writes the segment with the caller's hash
  (`chm_remap_seg_get` -> `chm_seg_get_raw`, which sees a reservation as the value it is;
  `chm_remap_seg_put` -> `chm_seg_put_hashed`; `chm_remap_seg_remove` -> `chm_seg_remove_hashed`),
  and `compute` / `computeIfPresent` / `merge` run `chm_seg_remap_hashed` under the segment
  monitor instead of the `HashMap` composite over the segment. One `hashCode()` each, as
  `spread(key.hashCode())` (were five for an absent `computeIfAbsent`, three for the others). The
  segment composite also threw `ConcurrentModificationException` when the function wrote another
  key of the same segment (its `modCount` check); the JDK has no such check, and
  `chm_seg_remap_hashed` makes none (its recursion rule, `chm_reject_recursive_update`, is
  unchanged). A reservation of another thread reads as absent, as for every lock-free reader.
* **Item 8, the `TreeMap` half** -- `CRATONVM_COMPAT_TREEMAP_VALUE_EQUALS_ORDER`. The `remove(k,
  v)` and `replace(k, old, new)` composites asked `value.equals(stored)` / `stored.equals(old)`
  (`HashMap`'s receivers) for a `TreeMap` too. `TreeMap` inherits `Map.remove(k, v)`
  (`Objects.equals(curValue, value)`: the stored value first) and overrides `replace(k, o, n)`
  (`Objects.equals(oldValue, p.value)`: the argument first); both are now asked that way round.
* Also landed from the views page (`r13w9-hashcompat4-map-views-call-hashcode-FIXED-20260928.md`, now
  FIXED): `keySet()` build/resync, view removals, entries that ARE the nodes, and a
  `values().iterator().remove()` wrong answer (it removed the first entry with an equal value).

### Still open

* **Item 3, the rest.** A write THROUGH a `LinkedHashMap.reversed()` view (`put`, `remove`, ...)
  changes the snapshot only, never the source (the JDK's view writes through); the view's class is
  `LinkedHashMap`, not `ReversedLinkedHashMapView`; its entries are the snapshot's nodes; and
  `LinkedHashSet.reversed()` (`native_lhs_reversed`) is still a snapshot that is never rebuilt.
  Proposal C5-1 in `jit-r13-compat5-proposals-RETIRED-20260929.md`.
* **Item 4, CHM order after a resize**: unchanged. It needs a per-map model of the JDK table (its
  bins and each `transfer`'s `lastRun` split), not only its size; the cheaper exit is still
  retiring the native CHM under `--compatible` (`jit-r13-hashtree-proposals-RETIRED-20260929.md` HT-3).
* **Item 5, CHM `equals` counts**: a present-key `computeIfAbsent` / `computeIfPresent` walks the
  segment chain twice (the lock-free probe, then under the monitor) and an absent-key
  `computeIfAbsent` three times (probe, reservation, commit), where the JDK walks the bin once;
  only a colliding user key sees the extra `equals` calls. Recorded in
  `r13w10-compat5-map-view-residuals-CLOSED-20260929.md`.
* **Item 8, the rest**: `TreeMap.replace(k, v)` / `replace(k, o, n)` still `get` + `put` (two
  tree walks, twice the `compareTo` calls of the JDK's one `getEntry`); `Properties` and
  `IdentityHashMap` were not re-derived this wave (`IdentityHashMap` has used `==` for values in
  `remove(k, v)` / `replace(k, o, n)` since JDK 20; not checked against the natives).

### How to confirm (wave 10)

* `cargo test -p cratonvm-native-collections --test r13_compat5_view_nodes` and the lib unit
  tests `dense_int_entries_tests::r13_compat5_*`, plus the wave-9 suites listed above.
* Probe `C:\craton\jitr13-probes\src\R13Compat5Views.java` (lines `chm-remap`, `lhm-reversed`,
  `treemap-eq` for this page), `--compatible`, default, and each switch at `0`.

## Round 13 wave 11 (lane compat6)

Each item re-read against the JDK 25 sources (`src.zip` of 25.0.3) and the current natives.

* **Item 3, reversed views.** Unchanged, and re-derived: `LinkedHashSet.reversed()`
  (`native_lhs_reversed`) builds a fresh `LinkedHashSet` from the source's list once; nothing
  marks it as a view, and the `HashSet`-shaped natives that read it (`size`, `iterator`,
  `contains`, `toString`, ...) have no resync hook like `resync_reversed_map`, so a rebuild on
  read would have to be added to each of them. The JDK's `ReverseLinkedHashSetView` also writes
  through (`add` -> `addFirst`, `remove` on the source). Both halves are proposal C5-1
  (`jit-r13-compat5-proposals-RETIRED-20260929.md`): one view carrier whose natives read the source's list
  backwards. Not done here.
* **Item 4, CHM order after a resize.** Unchanged (needs a model of the JDK table, or HT-3).
* **Item 5, CHM `equals` counts.** `compute` / `computeIfPresent` / `merge` now walk the segment
  chain once (`CRATONVM_COMPAT_CHM_REMAP_SINGLE_WALK`, see
  `r13w10-compat5-map-view-residuals-CLOSED-20260929.md`, "Round 13 wave 11"); `computeIfAbsent`'s
  reservation protocol still walks up to three times (recorded there).
* **Item 8, `TreeMap.replace` walks.** Re-derived and left: the native `TreeMap` is a sorted
  key/value array searched by binary search (`tm_binary_search`), not a red-black tree, so even a
  single search compares against different stored keys, in a different order, than the JDK's
  `getEntry` descent; halving the composite's two searches would not make any count HotSpot's.
  The count is exact only by retiring the `TreeMap` natives under `--compatible` (HT-3, the
  `TreeMap` family). `Properties`' `remove(k, v)` / `replace(k, o, n)` are its CHM's (the native
  `Properties` side table, unchanged). `IdentityHashMap` was checked and is not a native under
  `--compatible` at all: its natives (`register_identity_hashmap_natives`, `native-builtins`) are
  registered only by `register_synthetic_overrides`, which exists only in a `synthetic-jdk`
  build, so a real-JDK build runs `IdentityHashMap`'s own bytecode, identity value test
  included. (In a `synthetic-jdk` build they serve `<init>()V` with `native_em_init`, which
  refuses a missing key class, and `clone()` with `native_em_clone`, which answers an
  `EnumMap`; outside this round's scope, noted for whoever owns that build.)

## Round 13 wave 13 (lane compat7)

* **Item 5, done.** `ConcurrentHashMap.computeIfAbsent` now walks as the JDK body does
  (`CRATONVM_COMPAT_CHM_CIA_SINGLE_WALK`; details and counts in
  `r13w10-compat5-map-view-residuals-CLOSED-20260929.md`, "Round 13 wave 13"). Every call-count line of
  item 5 is closed.
* **Item 3, open (observable).** Unchanged; the entry points a live `LinkedHashSet.reversed()`
  would need are listed in `jit-r13-compat7-proposals-RETIRED-20260929.md` C7-2.
* **Item 4, open (observable after a resize).** Unchanged. The native CHM keeps no model of the
  JDK's bins, so `transfer`'s `lastRun` split (and, since wave 11, `TreeBin`'s prepend order for 8+
  colliding keys) cannot be reproduced; the exact cost of the alternative is on the new page
  `r13w13-compat7-chm-tree-bins-ht3-cost-20260928.md`.
* **Item 8, open.** Unchanged (`TreeMap` is a sorted array natively; HT-3, `TreeMap` family).

## Round 14 wave 1 (lane compat)

Status unchanged: OPEN. Left, all three re-checked in the code at `adb9178bc`:

* **Item 3, reversed views** -- still the observable residual (`LinkedHashMap.reversed()` writes
  do not reach the source; `LinkedHashSet.reversed()` (`native_lhs_reversed`) is a snapshot never
  rebuilt). It is now tracked HERE only: `r13w10-compat5-map-view-residuals-CLOSED-20260929.md` item 6 is
  the same defect and that page is CLOSED-PENDING. Not attempted: the fix is proposal C5-1 / C7-2
  (a view carrier, or a resync hook in each `HashSet`-shaped native), not a one-wave change.
  A cheaper route worth measuring first: retire `native_lhs_reversed` under `--compatible` so the
  JDK's own `ReverseLinkedHashSetView` bytecode runs; its methods call back into
  `LinkedHashSet` / `LinkedHashMap` operations the natives already serve, and its iterator walks
  the real `tail` / `before` fields the natives mirror. It needs the `LinkedHashMap.sequencedKeySet`
  / `LinkedKeySet(reversed)` path audited against the native map first (recorded as proposal
  COMPAT14-2 in `jit-r14-compat-proposals.md`).
* **Item 4, CHM order after a resize** and **item 8, `TreeMap` walk counts** -- both wait on the
  HT-3 owner decision (`r13w13-compat7-chm-tree-bins-ht3-cost-20260928.md`); unchanged.

Also landed this wave in the same family (see `r13w13-compat7-collection-review-findings-FIXED-20260929.md`,
"Round 14 wave 1"): `LinkedHashMap` / `LinkedHashSet` keep their load factor, a clone of an emptied
grown `LinkedHashMap` starts at 16 slots, and the native `HashSet` builder replays a tree that forms
in an intermediate table.

## Round 14 wave 2 (lane compat2)

**Item 3 re-read, not changed: not contained.** The cheap route (stop registering
`native_lhs_reversed` under `--compatible` so `LinkedHashSet.reversed()`'s own
`ReverseLinkedHashSetView` runs) was traced member by member against JDK 25:

* `iterator()` is `map().sequencedKeySet().reversed().iterator()`: `sequencedKeySet` is the
  native `native_lhm_key_set` and `LinkedKeySet.reversed()` is the native `native_view_reversed`,
  so each `iterator()` call would re-snapshot -- live per iterator, which is an improvement;
* `size()`, `add`, `addFirst/Last`, `getFirst/Last`, `removeFirst/Last` call the natives on the
  source -- live and write-through, correct;
* but `toArray()` / `toArray(T[])` are `map().keysToArray(.., true)` and the inherited
  `AbstractCollection` members use `iterator()`: `keysToArray` is REAL `LinkedHashMap` bytecode
  walking `tail` -> `before` on the heap nodes, while `lhm_get` documents that only `head` /
  `tail` / `size` / `table` are mirrored to the heap and the per-node links are kept in the
  native overlay (`lhm_overlay`). Whether `before` is current on every node after the native
  `put` / `remove` / `lhm_move_to_tail` paths is not provable by reading and cannot be measured
  in this lane (no build).

So the retirement needs one measurement first: `CRATONVM_UNRETIRE...`-style A/B of
`R13Compat5Views`' `lhs-reversed` line plus a `reversed().toArray()` row over a set that saw
`remove` + `addFirst`. Recorded as proposal CP2-2 in `jit-r14-compat2-proposals.md` with the
exact switch shape (the `optional_functional_left_to_bytecode` pattern). Items 4 and 8
unchanged. Status stays OPEN.

## Round 14 wave 3 (lane compat3)

**Item 3, the `LinkedHashSet` half: landed (CP2-2).** `register_hashset_natives`
(`native-collections/src/lib.rs`) no longer registers `native_lhs_reversed` on a real JDK in
`--compatible` (`lhs_reversed_left_to_bytecode`; kill switch
`CRATONVM_COMPAT_LHS_REVERSED_REAL_VIEW=0`, or `CRATONVM_UNRETIRE_NATIVE_SHADOW` naming the triple;
`--jdk-only` and the synthetic JDK unchanged). `LinkedHashSet.reversed()` is then the JDK's own
`ReverseLinkedHashSetView`. Wave 2's blocker was re-read and is not one: `toArray()` is real
`LinkedHashMap.keysToArray(.., true)` walking the heap `tail` / `before` chain, and that chain is
current -- every native list edit (`lhm_link_tail`, `lhm_link_head`, `lhm_unlink`,
`lhm_move_to_tail` / `_head`, all `lhm_append_distinct` / put / remove / poll paths) writes BOTH
node links on the heap (`LHM_NODE_BEFORE` / `_AFTER` are the real `LinkedHashMap$Entry` slots 4/5;
`rg 'LHM_NODE_AFTER' native-collections/src/lib.rs`: every write is paired with its `BEFORE`
twin), the JDK's own tree-bin code keeps them for tree nodes, and `lhm_set` mirrors `head` /
`tail` / `size` onto the real fields of every real-layout backing (`alloc_hs_backing` allocates a
real `LinkedHashMap`). `prepareArray` reads the mirrored `size`. What the view does now, member by
member (JDK 25 source):

| member | route | result |
|---|---|---|
| `size`, `add`, `addFirst/Last`, `getFirst/Last`, `removeFirst/Last` | `LinkedHashSet.this.*` natives | live, write-through (was: snapshot) |
| `iterator()` (and `contains`, `toString`, `equals`, `hashCode`, `containsAll`) | `map().sequencedKeySet().reversed().iterator()` = native keySet carrier + `native_view_reversed` | live per call: a fresh reversed snapshot per iterator |
| `toArray()`, `toArray(T[])` | real `keysToArray(.., true)` | live |
| `reversed()` | `LinkedHashSet.this` | the source itself (was: another snapshot) |
| `getClass()` | `LinkedHashSet$1ReverseLinkedHashSetView` | HotSpot's class (was `LinkedHashSet`) |

**Hand-back fix (w3a build):** the first `toString()` of the view died with
`IncompatibleClassChangeError: Class java.util.ArrayList does not implement the requested
interface java.util.SequencedSet` at `ReverseLinkedHashSetView.iterator` (LinkedHashSet.java:309):
`native_view_reversed`, registered on `LinkedHashMap$LinkedKeySet.reversed()` with a
`SequencedSet` return, answered an `ArrayList` (a latent type hole for any
`lhm.sequencedKeySet().reversed()` used through its declared type, now on the view's path). For a
key-set carrier over a `LinkedHashMap` it now answers a reverse-ordered `LinkedHashSet` built from
the source map's list and stored hashes (`lhm_key_set_view_source`, `lhs_reversed_snapshot_of`,
shared with `native_lhs_reversed`; no `hashCode()`), under the same switch. The
`LinkedEntrySet.reversed()` row keeps the same hole (an `ArrayList` for a `SequencedSet`); not on
this path, and a set of entries would call user `hashCode()`s -- C3-1 covers both.

Unit tests: `lhs_reversed_registration_tests` (lib.rs). Probe:
`C:\craton\jitr14-probes\src\R14Compat3LhsReversed.java` (`bad 0` on HotSpot; `--compatible`
should now match; with the switch at `0` it reports the snapshot).

**Still open in item 3** (unchanged or narrowed):

* A REMOVAL through the reversed set's iterator (`r.iterator().remove()`, and so
  `r.remove(o)`, `r.removeIf`, `r.retainAll`, `r.clear()`, which `AbstractCollection` builds on
  it) is applied to `native_view_reversed`'s `ArrayList` snapshot and never reaches the source;
  HotSpot's `LinkedKeyIterator.remove` removes the source node. (The old snapshot set had the
  same hole.) Fix: `native_view_reversed` on a `LinkedKeySet` carrier should hand back a
  reversed live carrier (or the real `LinkedKeySet(reversed = true)` with the native iterator
  reading `tail` / `before`); proposal C3-1 in `jit-r14-compat3-proposals.md`.
* `LinkedHashMap.reversed()` is still the rebuilt snapshot map (writes through the view never
  reach the source); proposal C3-2 (the same retirement, for `ReversedLinkedHashMapView`, after
  auditing its `put` / `entrySet` members).

**Item 4 / `R13HashcompatEqualsOrder` in `--compatible`.** That probe's only differing line is
`ConcurrentHashMap colliding` (`put=66/66/12/0` against HotSpot's `55/55/12/37`): the native CHM
has no tree bins, so a bin of 12 colliding `Comparable` keys is walked by `equals` where the JDK
treeifies and uses `compareTo`. It is item 4's cause (no model of the JDK's bins) and only a key
whose `equals` / `compareTo` has side effects can see it. **Accepted divergence** until the HT-3
owner decision (`r13w13-compat7-chm-tree-bins-ht3-cost-20260928.md`); no change this wave.

Items 4 and 8 unchanged. Status stays OPEN (item 3's removal-through-iterator and
`LinkedHashMap.reversed()` halves, items 4 and 8).

## Round 14 wave 4 (lane compat4)

**Item 3: both queued halves landed (C3-1, C3-2) and the entry-set type hole is closed.** All in
`native-collections/src/lib.rs`; `--jdk-only` and the synthetic JDK unchanged.

* **C3-1, live reversed key / entry sets** (`CRATONVM_COMPAT_LHM_REVERSED_LIVE_VIEWS`, default on).
  `native_view_reversed` on a native `LinkedHashMap$LinkedKeySet` / `$LinkedEntrySet` carrier now
  answers what JDK 25 answers, `new LinkedKeySet(true)` / `new LinkedEntrySet(true)`: a view
  carrier minted like the forward one (`mint_reversed_linked_view` -> `make_view_set_of`, a view
  backing that records the source, so every write goes through; not cached in the source's
  `keySet` / `entrySet` field) with its declared `reversed` field set; and `reversed()` of a
  reversed carrier answers the source's own `sequencedKeySet()` / `sequencedEntrySet()` (identity
  equal, as on HotSpot). Every reader honours the field (`linked_set_view_reversed`, asked before
  any allocation): `hs_view_elements` (iterator, `toArray`, `forEach`, `stream`, `spliterator`,
  `toString`, the element `removeIf`), `collect_collection_elements` (both carrier arms), the
  node-walking `removeIf` / `retainAll` / `removeAll` (`hs_remove_if_by_nodes`,
  `hs_filter_by_contains`: the predicate / `c.contains` run tail first), the iterator's node
  carrying (`itr_snapshot_nodes` matches the reversed node order, so `it.remove()` unlinks the node
  with no `hashCode()`), and the seeded real iterator fields (`reversed`, `next = tail`). The
  real-bytecode members (`getFirst` / `getLast` / `removeFirst` / `removeLast` read `reversed`,
  `head` / `tail` through `this$0`) now see the right field too. So `it.remove()`, `remove(o)`,
  `removeIf`, `retainAll`, `clear()` through `lhm.sequencedKeySet().reversed()` -- and through
  `LinkedHashSet.reversed()`, whose `iterator()` is exactly that path since CP2-2 -- reach the
  source. **The `LinkedEntrySet.reversed()` type hole is closed by the same arm**: it answers a
  `SequencedSet` (was an `ArrayList`); its backing starts empty and is rebuilt from the source on
  every read (entry views have no generation stamp), so no entry's `hashCode()` runs.
* **C3-2, `LinkedHashMap.reversed()` to `ReversedLinkedHashMapView`**
  (`CRATONVM_COMPAT_LHM_REVERSED_REAL_VIEW`, default on; also requires the C3-1 switch, since the
  view's `entrySet()` is `sequencedEntrySet().reversed()` and `AbstractMap.toString` walks it).
  `register_linked_hashmap_natives` no longer registers the snapshot closure on a real JDK in
  `--compatible` (`lhm_reversed_left_to_bytecode`, CP2-2's shape; `CRATONVM_UNRETIRE_NATIVE_SHADOW`
  naming the triple also restores it). Member audit against JDK 25 on the switch's doc comment:
  every `Map` / `SequencedMap` member delegates to `base.*` natives (live, write-through;
  `putFirst`/`putLast` and `poll*` swapped), `keySet()` / `entrySet()` are the C3-1 views,
  `forEach` / `replaceAll` are real bytecode over `base.tail`, `e.before`, `e.key`, `e.value`,
  `base.modCount` (all mirrored real heap state, wave 3's audit), `reversed()` is `base`. The class
  is now HotSpot's (`LinkedHashMap$ReversedLinkedHashMapView`). The rebuilt-snapshot machinery
  (`build_reversed_map_snapshot`, `resync_reversed_map`, the `__reversed_*` overlay rows) is now
  reached only by the synthetic JDK, the kill switches and the unmodifiable wrapper's `reversed()`
  (`UNMOD_MAP_CLASS`, out of scope); delete it once the switches are retired.

Unit tests: `lhm_reversed_registration_tests` (lib.rs). Probe:
`C:\craton\jitr14-probes\src\R14Compat4ReversedViews.java` (`bad 0` on HotSpot; `--compatible`
should match; `CRATONVM_COMPAT_LHM_REVERSED_LIVE_VIEWS=0` or `..._REAL_VIEW=0` report the old
answers).

**Still open in item 3 (narrowed):** `LinkedHashMap$LinkedValues.reversed()` -- and so
`lhm.reversed().values()` -- is still the reversed `ArrayList` snapshot of `native_view_reversed`
(a `SequencedCollection`, right when read; HotSpot: a live `LinkedValues(true)` whose removals
reach the source, class `LinkedHashMap$LinkedValues`). Not done here: the values carrier is
`ArrayList`-shaped and resynced by `resync_values_view`, and three node-indexed removal paths
(`values_view_node_at`, `values_view_remove_jdk`, the values `removeIf`) assume forward order;
each needs the reversed mapping. Proposal C4-1 in `jit-r14-compat4-proposals.md` with the list.

Items 4 and 8 unchanged (HT-3 owner decision). Status stays OPEN (item 3's values half, items 4
and 8).

## Round 14 wave 5 (lane compat5)

**Item 3, the values half: landed (C4-1).** `native-collections/src/lib.rs`, kill switch
`CRATONVM_COMPAT_LHM_REVERSED_LIVE_VALUES` (default on in both modes; `=0` restores the reversed
`ArrayList` snapshot; it also requires the wave-4 family switch
`CRATONVM_COMPAT_LHM_REVERSED_LIVE_VIEWS`). `--jdk-only` and the synthetic JDK are unchanged.

* `LinkedHashMap$LinkedValues.reversed()` (`native_view_reversed`) on a native values carrier over
  a `LinkedHashMap` now answers what JDK 25 answers, `new LinkedValues(true)`:
  `mint_reversed_linked_values` mints a values carrier exactly as `values()` does
  (`make_view_list_of`, source in the trailing marker slot, so removals go through) but does not
  cache it in the source's `values` field, sets its declared `reversed` (slot 0; the list state
  sits past both declared fields, `view_carrier_slots`) and fills it tail first. `reversed()` of a
  reversed carrier answers the source's own `sequencedValues()` (identity-equal, as on HotSpot).
  The class is HotSpot's `LinkedHashMap$LinkedValues` (was `ArrayList`).
* Every reader honours the field (`values_view_reversed`, a pure read asked before any
  allocation): `resync_values_view` (so iterator, `toArray`, `forEach`, `stream`, `toString`,
  `contains` all read tail first), the iterator's `remove()` node lookup (`values_view_node_at`:
  position `i` is node `n - 1 - i`), `remove(o)` (`values_view_remove_jdk`: the walk is tail
  first, so of two equal values the LAST in list order goes, as `AbstractCollection.remove` over
  the reversed iterator removes it), the node-walking `removeIf` (`native_al_remove_if`: the
  predicate runs tail first), and `vc_route` (an image-minted reversed `LinkedValues` is rebuilt
  as a reversed carrier; it iterated forward before).
* So `lhm.sequencedValues().reversed()` and `lhm.reversed().values()` (C3-2's view calls exactly
  that) are live and write-through: `m = {a=1, b=1}; m.sequencedValues().reversed().remove(1)`
  leaves `{a=1}` as on HotSpot (the snapshot left `{a=1, b=1}`).

Left as it was, stated: the by-VALUE fallbacks (`remove_source_entry_by_value` via
`propagate_list_removal`) still remove the FIRST equal value in list order. They are reached for a
`LinkedHashMap` source only with `CRATONVM_COMPAT_VALUES_REMOVE_JDK=0` or
`CRATONVM_COMPAT_VIEW_REMOVE_BY_NODE=0` (a `LinkedHashMap` is always a `plain_node_map`), and by
`retainAll` / `removeAll`, where every equal copy shares one verdict, so the result is the same set.

Unit test: `c4_1_linked_values_reversed_is_its_declared_field_and_nothing_else` (lib.rs, the
carrier mock: the mark lands on `reversed` @0 of a `LinkedValues` carrier only, never on a
one-field carrier's `this$0` or an `ArrayList`). Probe: `C:\craton\jitr14-probes\src\R14Compat5ReversedValues.java`
(`bad 0` on HotSpot; `--compatible` should now match; with the switch at `0` it reports the
snapshot answers).

**Items 4 and 8: re-read, not contained, unchanged.** Item 4 (CHM order after a resize, tree-bin
walk counts) needs a per-map model of the JDK's bins and each `transfer`'s `lastRun` split; item 8
(`TreeMap.replace` compare counts) is a sorted-array native whose binary search cannot reproduce
`getEntry`'s red-black descent. Both are the HT-3 owner decision
(`r13w13-compat7-chm-tree-bins-ht3-cost-20260928.md`); nothing a lane can land without it. Status
stays OPEN for items 4 and 8 only; item 3 is complete pending the orchestrator's probe run.
