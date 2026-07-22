# `java.util.TreeMap.tailMap()`/`headMap()` submap views are broken — FIXED

## Status
**FIXED** — dev@`fix/h2-treemap-tailmap-dispatch-20260721`, 2026-07-22. Root
cause fully identified and closed for all four overload forms (`headMap(K)`,
`tailMap(K)`, `subMap(K,K)`, and the 2-arg/4-arg `NavigableMap` forms
`headMap(K,boolean)`/`tailMap(K,boolean)`/`subMap(K,boolean,K,boolean)`).

**The original "leading hypothesis" in this doc (a `find_by_method_descriptor`
class-blind dispatch collision with `ConcurrentSkipListMap`) was wrong.**
`find_by_method_descriptor` has zero call sites anywhere in the interpreter —
it is dead-on-the-hot-path code, gated only for a documented, narrow recovery
case that never applies here. The actual bug lives entirely inside
`native-collections/src/lib.rs`'s synthetic `java.util.TreeMap` implementation
and has nothing to do with class dispatch, `ConcurrentSkipListMap`, or
receiver-type confusion.

## Root cause (confirmed)
CratonVM's synthetic `TreeMap` has two backing-store modes, chosen per-map
the first time an entry is inserted:
- **fast mode** — no custom `Comparator` and the key is natural-order-
  extractable (`String`/`Integer`/`Long`/other primitive wrappers): entries
  live in a Rust-side `BTreeMap` side-table (`tm_fast_table`), and only the
  entry **count** is mirrored into the synthetic `TM_FIELD_SIZE` slot so
  legacy size readers keep working.
- **array mode** — a custom comparator or a non-extractable key: entries live
  in a real `Object[]` backing array referenced by `TM_FIELD_DATA`.

The bug: `TreeMap`'s `<init>()`/`<init>(Comparator)` natives
(`native_tm_init`/`native_tm_init_comparator`) **unconditionally** allocate an
empty 32-slot backing array and store it in `TM_FIELD_DATA` at construction
time, before any entry is ever inserted and before fast-vs-array mode is
decided. For the common fast-mode case (the doc's own repro: `String` keys,
no comparator), every subsequent `put()` correctly stores into the
`tm_fast_table` side-table and mirrors only the **count**, but that original
empty array is never cleared or nulled out — it just sits there, unpopulated,
forever paired with a `TM_FIELD_SIZE` that (correctly) reflects the real
entry count.

`native_tm_head_map`/`native_tm_tail_map`/`native_tm_sub_map` read the source
map via `tm_state()`, which pulls `TM_FIELD_DATA`/`TM_FIELD_SIZE` directly —
the **same** helper `keySet()`/`values()`/`entrySet()`/`forEach()`/`iterator()`
correctly avoid by going through the fast/array-mode-aware `tm_collect_pairs()`
instead. For a fast-mode map, `tm_state()` therefore returns `(Some(<32 nulls>),
real_size, comparator)` — the real count, paired with `real_size` phantom
`null` keys read out of an array fast-mode `put()` never touched. Iterating
those phantom nulls:
- made **`headMap`** compare `null` against `toKey` on the very first
  (phantom) entry and immediately `break` — the "spuriously-empty view"
  symptom (`NoSuchElementException` on the very first `next()`).
- made **`tailMap`** repeatedly `put(null, <value>)` into the result for
  every phantom entry — each such put *replacing* the previous null entry
  (they all compare equal to each other), leaving exactly one corrupt
  `null`-keyed entry — the "first element is `null`" symptom.
- made **`subMap`** behave like `headMap` (empty), since its lower-bound
  check also sees `null` first.

The doc's own **standalone 2-arg repro** (`tailMap(K, boolean)`) hit a related
but distinct gap: `TreeMap`'s 2-arg `headMap`/`tailMap`/4-arg `subMap`
(`NavigableMap`-returning `boolean`-inclusive overloads) had **no native
registration at all** — only the 1-arg `SortedMap`-returning forms were
registered. Calls to the 2-arg forms fell through to real JDK bytecode
(`new AscendingSubMap<>(...)`), which navigates the real `root`/`comparator`
fields — fields a natively-managed `TreeMap` never populates (`root` stays
permanently `null`) — so every 2-arg view was unconditionally empty
regardless of the fast/array-mode bug above.

Confirmed via targeted `eprintln!` instrumentation in a debug build
(`--profile hc0053dbg`) before writing the fix: for a 5-entry fast-mode map,
`native_tm_tail_map` observed `data_opt.is_some()=true size=5 is_fast_mode=true`
and looped 5 times with `cmp=0` on every phantom-null comparison, ending with
`result size=1` — exactly matching the reported "first element null" symptom.

## Fix
`native-collections/src/lib.rs`:
- `native_tm_head_map`/`native_tm_tail_map`/`native_tm_sub_map` now read the
  source map via `tm_collect_pairs()` (the same fast/array-mode-aware helper
  `keySet`/`values`/`entrySet`/`forEach`/`iterator` already use) instead of
  `tm_state()` directly.
- Added the previously-unregistered 2-arg/4-arg `NavigableMap` forms —
  `native_tm_head_map_inclusive`/`native_tm_tail_map_inclusive`/
  `native_tm_sub_map_inclusive` — registered on both `java/util/TreeMap` and
  `java/util/NavigableMap`, mirroring the existing
  `native_ts_{head,tail,sub}_set_inclusive` pattern already used for
  `TreeSet`'s `NavigableSet` views.

`java.util.TreeSet` was checked and does **not** have this bug: it has no
fast/array dual-mode (its `ts_state` array is the sole backing store), so its
existing `tailSet`/`headSet`/`subSet` (inclusive and non-inclusive) views were
already correct.

## Verification
- Doc's own standalone repro (`tailMap("key1", true).keySet().iterator().next()`
  for a 20-entry map) now prints `key1` (matches HotSpot), was `null`.
- All four overload forms (1-arg `SortedMap`, 2-arg/4-arg `NavigableMap`)
  verified correct at map sizes 5/10/20/50 against expected lexicographic
  `String` ordering.
- `org.h2.test.unit.TestReopen`, `org.h2.test.db.TestPowerOff` — PASS (both
  previously NPE'd at `FilePathMem.newDirectoryStream` via
  `MEMORY_FILES.tailMap(name)`).
- `org.h2.test.synth.TestDiskFull`, `org.h2.test.synth.TestPowerOffFs` — the
  reported NPE is gone (no exception in the log at all; both now run well
  past the point that used to fail, doing hundreds of real write/crash-sim
  iterations).

  **Residual timeout, chased and closed (2026-07-22, NOT A BUG):**
  `TestDiskFull` completes cleanly under CratonVM in ~100s (380 write-op
  iterations; `--java-home` JIT mode, 300s budget) vs. ~17s under real
  HotSpot (JDK25, 230 iterations) — well inside any reasonable per-class
  timeout, ~6x overhead in line with CratonVM's normal interpreter/JIT gap
  for this kind of workload.

  `TestPowerOffFs` is a different story, but not a CratonVM bug: its
  `test()` method has an *unbounded* `for (int i = 0;; i++)` loop (no
  upper limit — see `apps/h2database/h2/src/test/org/h2/test/synth/
  TestPowerOffFs.java`) that only terminates once `i` exceeds the total
  number of internal write operations performed by one full
  create/insert/update/delete/drop DB lifecycle, with each iteration
  re-running that entire lifecycle from scratch. Confirmed via direct
  standalone runs on the Azure host: **real HotSpot itself does not finish
  this test within 10 minutes** (`timeout 600` → killed, rc=124, `real
  10m22s` / `user 1m52s` / `sys 4m30s` — heavy syscall time, consistent
  with H2's `FilePathDebug` wrapper doing large numbers of small real I/O
  ops per simulated write). CratonVM shows the same shape of behavior
  (completes the bounded first phase, then times out mid-way through the
  identical unbounded second loop; `timeout 180` → rc=124, no crash, no
  exception, no CratonVM-specific divergence from HotSpot's behavior).
  This is upstream H2 test-design cost (an intentionally exhaustive fuzz
  loop with no cap), not a VM correctness or performance bug — same
  disposition as this suite's separate raw-Thread hang-cluster
  investigation (`apps/h2database-suite-runner/RESULTS-20260722-rawthread-
  hang-investigation.md`): slow-by-design test, not a VM defect. No source
  change made; closing this residual as confirmed not-a-bug.

## Repro (kept for reference — now fixed)
```java
import java.util.TreeMap;
public class T {
    public static void main(String[] a) {
        TreeMap<String,Integer> m = new TreeMap<>();
        for (int i = 0; i < 20; i++) m.put("key"+i, i);
        System.out.println(m.tailMap("key1", true).keySet().iterator().next());
    }
}
```
