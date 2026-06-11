# Code Review — `native-collections` crate

Reviewer: Fable (Opus 4.8) · Date: 2026-06-10 · Scope: `native-collections/src` (2 files, ~26.7k LOC) + `native-collections/tests` (6 files).

## Summary

`native-collections` implements Rust-backed native intrinsics for the `java.util.*` and `java.util.concurrent.*` container classes. The bulk of the logic lives in one 26,658-line `lib.rs`; `identity_hash.rs` is a 58-line side-table-key helper. Per the README the crate is "the synthetic alternative used by the minimal test harness", but in practice `register_collections_natives` registers nearly all natives **unconditionally** (only the `BlockingQueue` family and a handful of inline tests are `synthetic-jdk`-gated), so most of this code is live in default builds as `Bridge`-category natives.

Overall code quality is high and notably defensive: index handling rejects negatives before the `usize` cast, capacity growth is capped, range materialization uses widening arithmetic with an explicit element cap, the CHM resize path has cycle guards plus a striped-RwLock + volatile-publish concurrency model, and there is extensive in-code documentation of prior bugs. The riskiest area is the family of **process-global side-tables** (overlays, fast-table, obj-key registry) that hold heap `Value`s/`ObjectRef`s and interact with the moving GC. I found one GC-soundness gap there (fast-mode TreeMap values are neither GC roots nor remapped), several natural-ordering correctness bugs (custom `Comparable` keys compare as equal; `Collections.sort(List)` only sorts strings), active behavior-faking stubs in the scheduled-executor path, and unbounded growth of the global side-tables. No memory-unsafety was found in the `unsafe` blocks (they are minimal and well-reasoned).

Files **fully read**: `identity_hash.rs`; `tests/mock_hashmap.rs`, `tests/mock_treemap.rs`, `tests/gc_relocation_harness.rs`; structural map of all of `lib.rs`. Files **sampled** (deep-read of high-risk regions, skimmed elsewhere): `lib.rs` — deep-read of obj-key registry/pack (L60-121), ChmMonitorGuard unsafe (L439-501), map hash/equal/bucket/resize (L1882-2400), ArrayList state/capacity/get/set/add (L681-1062), GC overlay scan/update (L16126-16215), TreeMap fast/array side-tables + binary search (L15806-16573), comparator/natural_compare (L10820-10945), Collections.sort + Arrays.sort + compare_via_compare_to (L5651-5963), Properties parser (L20123-20404), blocking-queue (L22142-22261), CHM get/put (L18952-19120), range materialization (L9522-9594), scheduled-executor (L22859-23024, L24130-24246); structural skim of streams/collectors/optionals/iterators. `tests/mock_arraylist.rs` and `tests/mock_lhm_access_order.rs` headers + test-name inventory only.

---

## Bugs

### B1 (HIGH) — Fast-mode TreeMap values are not GC roots and are never remapped → use-after-free / stale pointers
`tm_fast_table()` (L15851) is a `Mutex<StdHashMap<usize, BTreeMap<TreeKey, Value>>>`. The `Value` stored as the map *value* (set via `bt.insert(tk, value)` in `native_tm_put`, L16506) can be `Value::Object(Some(ref))` — e.g. a `TreeMap<String,SomeObject>` in fast mode. But `gc_scan_collection_overlay_roots` (L16126) and `gc_update_collection_overlay_refs` (L16169) scan/remap only the LL, LHM, `tm_array_table`, and `ts_array_table` tables — **`tm_fast_table` is omitted entirely**. Consequence: a fast-mode TreeMap value that is reachable only through the map is (a) invisible to the GC root scan → can be collected while still logically held → use-after-free on the next `get`, and (b) not repointed after a moving GC → stale/torn pointer. `TreeKey::Str/I32/I64` keys are by-value Rust data (safe), so only values are affected, but values are exactly the data users store. This is the one genuine soundness gap in the crate.

### B2 (HIGH) — `natural_compare` returns 0 for arbitrary `Comparable` objects → TreeMap/TreeSet ordering silently collapses
`natural_compare` (L10916) handles String (by content) and primitive-wrapper field-0 values, but for any other `(Object, Object)` pair it falls through to `_ => Ok(Some(Value::Int(0)))` (L10935) — i.e. "equal" — instead of dispatching `compareTo`. `tree_compare` (L16263) calls `natural_compare` whenever no `Comparator` is set, and the array-mode TreeMap/TreeSet path is reached precisely when keys are *not* fast-extractable (custom `Comparable` objects). So a comparator-less `TreeSet<MyComparable>` / `TreeMap<MyComparable,V>` treats all elements as equal → wrong ordering, lost entries, broken dedup. The correct helper (`compare_via_compare_to`, L5781) exists and is used by `Arrays.sort(Object[])` but was never wired into `natural_compare`. Also affects `Comparator.naturalOrder()`/`Comparator.comparing(keyExtractor)` (L10852, L10881) when the extracted key is a custom Comparable.

### B3 (HIGH) — `Collections.sort(List)` only sorts by string key; non-String lists are not sorted
`native_collections_sort` (L5937) builds a `Vec<(String, Value)>` where the sort key is `read_string(obj).unwrap_or_default()` (L5953) and sorts by that string. For `List<Integer>`, `List<Date>`, or any custom `Comparable`, the key is `String::new()` for every element, so `items.sort_by(|a,b| a.0.cmp(&b.0))` is a stable no-op — the list is returned **unsorted**. The sibling `Arrays.sort(Object[])` (L5651) was fixed to use `compare_via_compare_to` with a Comparable check, but the `Collections.sort` 1-arg path was not updated. (The comparator overload `sort_with_comparator`, L6378, is correct.)

### B4 (MEDIUM) — Global side-tables grow without bound (no removal on collection death) → memory leak
`obj_key_registry` (L78), `ll_overlay`, `lhm_overlay`, `tm_array_table`, `ts_array_table`, `tm_fast_table`, `tm_force_array_set`, `lhm_ptr_cache` are all process-global maps keyed by identity hash, with **no eviction path** when the owning collection becomes unreachable. Every LinkedList/LHM/TreeMap/TreeSet ever created (and every object that ever produced an `obj_key`) leaves a permanent entry. For long-running apps that churn many such collections this is an unbounded native-memory leak independent of the Java heap. There is no finalizer/weak-key hook.

### B5 (MEDIUM) — `widened_obj_key` exact-pointer fast path can alias a dead object's overlay after address reuse
In `widened_obj_key` (L88) the first check is `slots.iter().find(|s| s.last_ptr == ptr)` (L96), returning the stored generation. Because registry entries are never removed (see B4) and `last_ptr` is a raw address, if object A (hash H, ptr P) is collected and a later object B is allocated at the *same address P* and the VM assigns it the same identity hash H, `widened_obj_key(B)` matches A's stale slot by exact pointer and returns A's generation → B aliases A's old overlay entry (stale size/head/tail/data). The single-slot relocation path (L103) has the same exposure. The `gc_relocation_harness` only exercises live, unique-hash objects so this case is untested. Likelihood depends on the VM's identity-hash derivation (out of scope), but the design is not robust to address reuse + hash collision.

### B6 (MEDIUM) — `ScheduledThreadPoolExecutor.scheduleAtFixedRate` / `scheduleWithFixedDelay` run the task exactly once → periodic semantics silently dropped
`native_stpe_schedule_fixed_rate` (L22987) and `native_stpe_schedule_fixed_delay` (L23002) `invoke_virtual(runnable,"run")` exactly once, synchronously, ignoring the period and returning a completed future. These are registered live via `register_executors_scheduled_natives` (L24173/24180). Any app that relies on periodic execution (heartbeats, poll loops, metric flushers) gets a single run and then silence — a behavior-faking stub. See also Stubs S1. (Also a bug, not just a stub: the documented contract is periodic.)

### B7 (LOW) — `obj_to_display_string` `char::from_u32(v as u32)` can replace a valid Character with `?`
At L528, a boxed `Character` is formatted via `char::from_u32(v as u32).unwrap_or('?')`. A surrogate-range or out-of-range int (possible if the wrapper holds a malformed value, or for the `Character` MAX edge) prints `?` instead of the real char. Minor display fidelity issue.

---

## Vulnerabilities

No memory-safety vulnerability was confirmed. The crate has very little `unsafe`, and untrusted-length handling is consistently guarded. Items below are the soundness-adjacent observations worth tracking.

### V1 (HIGH) — GC root/remap gap is a soundness hazard (cross-listed with B1)
The omission of `tm_fast_table` from `gc_scan_collection_overlay_roots`/`gc_update_collection_overlay_refs` is a memory-safety issue under a moving GC: stored object values can be freed or left dangling. Treated as the primary soundness finding.

### V2 (LOW) — `ChmMonitorGuard::acquire` lifetime-transmute relies on a caller discipline the type system only partially enforces
`acquire` (L453) does `core::mem::transmute` of the `&mut dyn NativeContext` to `'static` and stores it as a raw pointer, with a `PhantomData<fn() -> &'a mut …>` to pin the lifetime. The safety argument (guard is always a local, dropped before the borrow ends) is sound for current call sites, but the invariant is not machine-checked — a future refactor that stores the guard, or reorders so `ctx` is dropped/moved before the guard, would be UB. The `Drop` impl already defensively `catch_unwind`s `monitor_exit` (L492). Documented as a hazard, not an active bug.

### V3 (LOW) — Properties parser fully materializes input into a `String` with no size cap
`props_read_input` (L20296) reads the entire backing byte array into a `Vec`/`String`; `props_parse_logical_lines` (L20123) splits into `raw_lines` (one `&str` per line). A maliciously large `.properties` stream causes proportional native allocation with no upper bound (unlike `range_*` which are capped). DoS-by-OOM only; no corruption. Parsing itself is bounds-safe (verified `decode_escape`/`utf8_char_end` guard all indexing).

---

## Stubs and Unimplemented

This crate's policy-relevant stubs are natives that **fake app behavior**. Found:

- **S1 (active)** `native_stpe_schedule` (L22972), `native_stpe_schedule_fixed_rate` (L22987), `native_stpe_schedule_fixed_delay` (L23002): run the runnable once, synchronously, ignoring delay/period; return an already-completed future. Registered live (L24169/24175/24181). The fixed-rate/fixed-delay ones are outright wrong (B6); the delayed `schedule` drops the delay.
- **S2 (active)** `native_executors_new_scheduled_pool` (L24228) / `native_stpe_init` (L22956): build a synthetic 3-field executor whose "task list" array is allocated but never used (no real scheduling/threading). Stat/policy setters (`setKeepAliveTime`, `setRemoveOnCancelPolicy`, …) are no-op recorders (`native_stpe_ignore_policy_setter`). Faked executor.
- **S3 (active)** `native_stpe_submit_runnable` (L23047) / `native_stpe_submit_callable` (L23060): execute synchronously on the calling thread and wrap in a completed future — not a real async submit.
- **S4 (dead code)** `register_scheduled_executor_natives` (L22859) is defined but never called from `register_collections_natives` (intentionally removed, per comment L269-274). Recommend deleting it and the inline `native_stpe_*` it referenced if now duplicated, to avoid confusion.
- **S5 (intentional, documented)** `native_ll_listitr_remove_noop` (L12218) and `native_itr_remove_noop` (L22605): no-op iterator removes. These appear to be deliberate fallbacks; flag for confirmation they aren't silently swallowing real removals.
- **S6 (simplification)** ConcurrentSkipListMap is an array-backed sorted map under a striped lock, not a real skiplist (documented TODO L23092); `native_chm_for_each_parallel` (L19588) is sequential. Functionally correct, performance-faking.

No `unimplemented!`/`todo!`/`NotImplemented` macros are present in this crate.

---

## Performance

- **P1 (HIGH)** `LinkedBlockingQueue` poll is O(n) per element → producer/consumer loops are O(n²). `lbq_poll_locked` (L22199) shifts every remaining element left one slot on each dequeue (L22213-22216). For a queue used as an actual FIFO this is quadratic; a circular head/tail index (as `ArrayDeque` already uses) would make it O(1).
- **P2 (MEDIUM)** `Collections.sort`/`List.sort`/`Arrays.sort(Object[])` use insertion sort — O(n²). `sort_with_comparator` (L6400) and `native_arrays_sort_objects` (L5718) both insertion-sort. The comments justify this (fallible comparator can't use Rust `sort_by`), but a merge sort with early-exit on `Err` would restore O(n log n) and is worth it for large lists.
- **P3 (MEDIUM)** Per-call global-lock acquisition on every TreeMap/TreeSet slot access. `tm_get_slot`/`tm_set_slot` (L15904/15924), `tm_is_fast_mode` (L15954), `tm_has_no_comparator`, `tm_force_array_mode` each take `tm_*_table().lock()` and recompute `widened_obj_key` (itself a `obj_key_registry().lock()` + linear bucket scan). A single `native_tm_put` can take 5+ global-mutex round-trips; under multi-threaded TreeMap use these serialize across *all* TreeMaps in the process.
- **P4 (MEDIUM)** `widened_obj_key` (L88) does a linear `slots.iter().find` on every overlay key lookup. For the (rare) multi-slot collision buckets this is fine, but it is on the hot path of every LL/LHM/TM/TS operation and always takes the global registry lock.
- **P5 (LOW)** `obj_to_display_string` (L508) calls `class_name_of_id`/`class_name_of_object` and string `.contains("Boolean")`/`.contains("Character")` on every wrapper toString — repeated substring scans on a hot formatting path (`toString` of any collection of wrappers).
- **P6 (LOW)** `native_props_store` (L20349) and several `to_string` natives build results with repeated `push_str(&format!(...))`; `format!` allocates a throwaway `String` per element. Use `write!` into the buffer or direct `push_str` of pre-rendered parts.
- **P7 (LOW)** `native_map_remove`/`map_keys_equal` walk chains invoking `equals` per node; fine, but `map_collect_*`/`props_collect_keys` allocate a fresh `Vec` and re-walk all buckets on each `keySet()/values()/entrySet()` call with no caching.

---

## Tests

**Estimated coverage: ~35%.**

Basis: tests fall into two buckets. (1) Inline `#[cfg(test)]` module (L25254-26658): mostly **registration-completeness** checks (does the registry contain class/method/descriptor X) plus a few real helper unit tests (`map_bucket_index` edge cases, `values_equal`, field-layout-constant consistency, `range_int/long_elements` overflow caps) and four genuinely concurrent `LinkedBlockingQueue` tests (blocking put/take, lost-value race, clear-unblocks-put) driven through a heap-backed `MockCtx`. (2) Integration tests in `tests/`: behavioral coverage for HashMap (put/get/overwrite/remove/resize/keySet-iteration/tail-append-order/merge+compute boxing — strong), ArrayList (add/get/size/remove/iterator/sublist/AIOOBE — good), TreeMap (Integer-key sorted iteration + round-trip), LinkedHashMap access-order (4 tests), and a GC-relocation harness asserting overlay **key stability** across a simulated move for LL/LHM/TM/TS.

What has *real behavioral* coverage: HashMap core + resize, ArrayList core, TreeMap fast-mode (Integer), LHM access-order, LBQ blocking/concurrency, overlay key stability, range overflow. That is a meaningful but narrow slice of a ~26k-LOC surface spanning ~40 container families.

**Does it plausibly reach 85%? No.** Entire families have zero behavioral tests: streams (the largest single section — filter/map/collect/reduce/sorted, int/long/double streams), Collectors, Optional family, Comparator/`natural_compare`, `Collections.sort/shuffle/sort-comparator`, PriorityQueue, ArrayDeque, Vector/Stack, Properties parsing, ConcurrentHashMap segment routing/resize, CSLM, the unmodifiable wrappers, and the scheduled-executor/CompletableFuture concurrency. The GC-soundness functions (`gc_scan_collection_overlay_roots`/`gc_update_collection_overlay_refs`) are not exercised end-to-end at all.

Most important missing tests (would also have caught findings above):
1. Comparator-less TreeMap/TreeSet with a **custom `Comparable`** key — would expose B2 (`natural_compare` returns 0).
2. `Collections.sort(list_of_integers)` ascending assertion — would expose B3.
3. GC root/remap round-trip for a **fast-mode `TreeMap<String,Object>`**: store object values, simulate a move via `gc_update_collection_overlay_refs`, assert values survive — would expose B1.
4. `ScheduledExecutor.scheduleAtFixedRate` invoked N times asserting N runs — would expose B6/S1.
5. Stream pipeline behavioral tests (`filter→map→collect(toList)`, `reduce`, `sorted` with comparator) and Collectors (`groupingBy`, `toMap` merge).
6. ConcurrentHashMap concurrent put/get/resize race test (mirrors the LBQ concurrency tests) and `containsValue`/`entrySet` consistency.
7. PriorityQueue sift-up/down ordering with a comparator; ArrayDeque wrap-around `removeFirstOccurrence`.
8. `widened_obj_key` aliasing after a simulated free + address reuse (B5).

The existing tests are well-written (heap-backed mock, runaway guards, HotSpot-parity assertions). The gap is breadth, not depth.

---

## Feature Suggestions

1. **Wire `natural_compare` and `Collections.sort(List)` through `compare_via_compare_to`** so custom `Comparable` types order correctly everywhere (fixes B2, B3 with a shared helper; add the Comparable-check + ClassCastException path already present in `Arrays.sort`).
2. **Register `tm_fast_table` (and audit every global side-table) with the GC scan/remap functions**, ideally via a single `for_each_overlay_value` iterator so a newly added table can't be forgotten again. Add a compile-time/test assertion enumerating all overlay tables.
3. **Weak-keyed / reapable side-tables**: give the overlay registry a way to drop entries when the owning object dies (e.g. a GC callback that prunes keys absent from the live set, or store overlays in object fields where layout permits). Fixes the unbounded-growth leak (B4) and the address-reuse aliasing (B5).
4. **O(1) `LinkedBlockingQueue`** via circular head/tail indices (reuse the `ArrayDeque` AD_FIELD_HEAD/TAIL pattern) to remove the O(n²) poll path (P1).
5. **Real `ScheduledExecutorService`** (or explicit `NotImplemented` rather than fake single-run) — at minimum make `scheduleAtFixedRate`/`scheduleWithFixedDelay` either delegate to the native-builtins scheduled pump or throw, per the no-synthetic-stubs policy (B6/S1).
6. **Cap and stream the Properties reader** (bounded buffer + incremental line parse) so a hostile `.properties` input can't OOM the VM (V3), and add fuzz coverage for the escape/continuation decoder.
