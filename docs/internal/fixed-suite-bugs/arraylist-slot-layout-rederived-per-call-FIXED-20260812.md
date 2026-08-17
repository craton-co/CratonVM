# `al_slots_for` re-derived the list slot layout on every collection call

**Status:** ✅ **FIXED** 2026-08-12 on `fix/netty-batch03-20260812`.
Found by triaging [netty investigate-batch-03](../../known-issues/netty/investigate-batch-03.md)
on the Azure Linux host (`20.80.105.49`), binary built from `origin/dev`
`95a156234`.

Batch-03's only failure was
`io.netty.buffer.search.SearchProcessorTest [3] AHO_CORASIC`, and it turned out
not to be a netty problem at all: `java.util.ArrayList` operations cost ~1 µs
each on CratonVM.

## The measurement chain

`SearchProcessorTest` fails one parameterized case — `testUniqueLen64Substrings`
for AHO_CORASIC — on JUnit's 120 s per-test cap. Its two sibling algorithms
pass. A standalone probe over the same workload (2016 factories, 64-byte
needles) isolates the outlier; **build only**, no netty test harness:

| algorithm | HotSpot | CratonVM (before) | ratio |
| --- | --- | --- | --- |
| KMP | 2 ms | 39 ms | 20× |
| SHIFTING_BITMASK | 1 ms | 44 ms | 44× |
| **AHO_CORASIC** | **214 ms** | **217 477 ms** | **1016×** |

20–44× is this VM's ordinary interpreted gap. 1016× is not, so something in the
Aho-Corasick path specifically was being hit.

`--stack-sample-ms 20 --nojit` put ~99 % of interpreted time in
`AhoCorasicSearchProcessorFactory.buildTrie`, with the leaf frame **in
`buildTrie` itself** and no callee frames beneath it — the signature of native
calls, which never push an interpreter frame. `--dump-native-registry` then
named them exactly, for 60 factories:

```
java/util/ArrayList  get(I)          2 025 308
java/util/ArrayList  size()I         2 016 336
java/util/ArrayList  add(Object)Z    2 005 270
                                     ---------
                     TOTAL all natives  6 110 155      (ArrayList = 99 %)
```

6.05 M ArrayList native calls in 6.46 s ≈ **1.07 µs per call**. `buildTrie`
holds a 256-entry-per-node `ArrayList<Integer>`, so a 64-byte needle is ~16 640
`add`s plus a `size()`+`get()` copy loop — the volume is real, the per-call cost
was not.

A scaling probe showed the cost is **flat in list length** (100 → 64 000
elements), so this was never an algorithmic O(n) bug — just a very large
constant. And a control that implements `size()`/`get()` in plain Java, on the
same VM, settled where the constant lives:

| | HotSpot | CratonVM before | CratonVM after |
| --- | --- | --- | --- |
| `raw int[] read` | 6 ns | 18 ns | 17 ns |
| `MyList.size()` — plain Java | 1 ns | **28 ns** | 27 ns |
| `ArrayList.size()` — native | 1 ns | **1014 ns** | **337 ns** |
| `MyList.get()` — plain Java | 2 ns | 63 ns | 62 ns |
| `ArrayList.get()` — native | 3 ns | **1107 ns** | **428 ns** |
| `Integer.valueOf` (cached) | 2 ns | 84 ns | 81 ns |
| `Math.max` | 2 ns | 63 ns | 62 ns |

The interpreter was fine (28 ns for a plain method call). `Math.max` and
`Integer.valueOf` natives were fine (60–85 ns). Only the collection natives cost
a microsecond — **36× more than the identical method body written in Java.**

## Root cause

`al_slots_for` resolves which slots hold `elementData`/`size` for the receiver,
and sits on the path of every `native_al_*` entry point. It re-derived that
constant on every call:

* `ctx.class_id_by_name("java/util/Vector")` — string-keyed class lookup,
* `ctx.is_subclass(cid, vec_id)` — hierarchy walk,
* two `ctx.resolve_field_index("java/util/ArrayList", …)` — string-keyed field
  lookups,

each behind the class-manager read lock. `native_al_size` reaches it **twice**
per call (once directly, once via `resync_values_view` →
`values_view_source` → `al_state`). `unmod_receiver_backing` added a
`class_name_of_id`, which takes the same lock and allocates a fresh `String`
per call — despite `class_name_rc`, the memoized reader that exists a few
hundred lines away, being written for exactly that reason and wired into only
2 of 52 call sites.

## The fix

Memoize the layout per receiver `ClassId` in a thread-local direct-mapped
cache, modelled on the `RECEIVER_FACTS` / `RECEIVER_NAMES` caches already in
this file and sound for the same stated reason: a `ClassId` is never reissued,
and a class's field layout is immutable after linking. `unmod_receiver_backing`
and `singleton_wrapper_size` now use `class_name_rc`.

**Only fully resolved layouts are cached.** `resolve_field_index` can fail
before the class is loaded and succeed afterwards, so caching the constant
fallback would pin the wrong slots for the rest of the run — the same reasoning
`class_name_rc` documents for not caching its `None`. `al_slots_for_uncached`
returns `Option`, and a Vector receiver whose own layout will not resolve
reports unresolved rather than falling through to ArrayList's `size` slot,
preserving the DF08 Vector/ArrayList separation the function exists for.

## Effect

| | before | after |
| --- | --- | --- |
| `ArrayList.size()` | 1014 ns/op | **337 ns/op** |
| `ArrayList.get()` | 1107 ns/op | **428 ns/op** |
| AC `buildTrie`, 2016 factories | 217 477 ms | **118 358 ms** |
| `SearchProcessorTest` solo | 255 s, **14/15** | **106–111 s, 15/15** (twice) |

## Honest residual

**`SearchProcessorTest` passes solo but still fails under the suite's 3-way
sharding** (135 s, tripping the same 120 s per-test cap). The remaining ~12×
over the plain-Java equivalent is structural and is filed as
[`arraylist-native-overhead-and-view-carrier`](netty/arraylist-native-overhead-and-view-carrier-FIXED-20260813.md):
CratonVM returns `map.values()` as an object whose class **is exactly
`java.util.ArrayList`**, so the natives cannot take an exact-class fast path
and must run the view/wrapper discrimination chain on every call. That page
carries the evidence and the suggested direction.

## Tests

* `al_slots_memo_matches_the_uncached_path_and_is_stable`
* `al_slots_memo_does_not_cross_wire_two_receiver_classes`
* `al_slots_memo_does_not_cache_the_fallback` — asserts a late-resolving layout
  still wins, which is the failure mode a naive memo would introduce.

Writing those exposed a second defect: `MockCtx::resolve_field_index(name,
field)` was a `None` stub while `resolve_field_index_by_class_id` right above it
was fully modelled, so a class a test had just described with `define_class` +
`define_field` still answered "no such field". That silently forces
`al_slots`/`al_slots_for` onto the constant-fallback path in **every** unit
test — a layout test could not have steered away from it. Now delegates to the
by-id resolver. `cargo test -p cratonvm-native-collections --lib`: 111 passed,
0 failed (108 before, +3 new).

Behavioural cover beyond the unit tests: a 37-assertion probe over every
receiver shape that reaches this family — ArrayList, Vector, Stack, subclasses
of each, `unmodifiableList`, `singletonList`, `emptyList`, `List.of`, a map
`values()` view (including resync after a later `put`), `keySet`,
`ConcurrentHashMap.newKeySet`, `subList`, and `sort` — 37/37 on CratonVM,
matching HotSpot 37/37.
