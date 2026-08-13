# Collection views were carrier-classed `java.util.ArrayList`, and every ArrayList call paid for it

**Status:** ✅ FIXED 2026-08-13. Both symptoms are closed and the netty class
this was filed against passes. What is left over is narrower than what this page
opened with and is re-filed as
[collection-view carrier residuals](../../../known-issues/collection-view-carrier-residuals-20260813.md).

Filed 2026-08-12 as the residual behind netty investigate-batch-03's last
failure, after the slot-layout memo landed
([record](../arraylist-slot-layout-rederived-per-call-FIXED-20260812.md)).
Fixed on the Azure Linux host (`20.80.105.49`), from `origin/dev` `ae2e1d9c8`.

## The two symptoms, and what they are now

CratonVM fabricated a map's `values()` view as an object whose runtime class was
**exactly `java.util.ArrayList`**. Measured against a HotSpot JDK 25 oracle with
`probes/ViewClassProbe`:

| expression | HotSpot JDK 25 | CratonVM before | CratonVM after |
| --- | --- | --- | --- |
| `hashMap.values().getClass()` | `java.util.HashMap$Values` | **`java.util.ArrayList`** | `java.util.HashMap$Values` ✅ |
| `linkedHashMap.values().getClass()` | `…$LinkedValues` | **`java.util.ArrayList`** | `…$LinkedValues` ✅ |
| `treeMap.values().getClass()` | `java.util.TreeMap$Values` | **`java.util.ArrayList`** | `java.util.TreeMap$Values` ✅ |
| `treeMap.entrySet().getClass()` | `java.util.TreeMap$EntrySet` | **`java.util.ArrayList`** | `java.util.TreeMap$EntrySet` ✅ |
| `chm.values().getClass()` | `…ConcurrentHashMap$ValuesView` | **`java.util.ArrayList`** | `…$ValuesView` ✅ |
| `hashMap.values() instanceof List` | `false` | **`true`** | `false` ✅ |
| `hashMap.values().equals(anArrayList)` | `false` | **`true`** | `false` ✅ |
| `hashtable.values().getClass()` | `…$SynchronizedCollection` | **`java.util.ArrayList`** | `java.util.Hashtable$ValueCollection` ⚠️ |
| `hashMap.keySet().getClass()` | `java.util.HashMap$KeySet` | `java.util.HashSet` | unchanged ⚠️ |
| `arrayList.subList(0,2).getClass()` | `java.util.ArrayList$SubList` | `cratonvm.internal.ArrayListSubList` | unchanged ⚠️ |

The ⚠️ rows are the residuals; see the linked page.

**Symptom 2 — the expensive one — is closed by the same change.** Because a
plain `java.util.ArrayList` receiver *might* have been a values view,
`native_al_size` and friends had to run the full discrimination chain on every
call. That is what the carrier change was for, and the page's original
instruction — *"Do not add that fast path first"* — held: the fast path landed
only after the views stopped being ArrayList-classed.

## Measured

`probes/AlCallCostProbe`, Azure host, ABBA-interleaved, `sink` identical in
every arm. The **right-hand column is the number to quote**: the same method
body written in plain Java, measured in the same process, so it is immune to
this box's load (which moved by 3x during these runs).

| ns/op | before | after | vs plain Java, before → after |
| --- | ---: | ---: | --- |
| `ArrayList.size()` | 580–634 | **331–345** | 29× → **16.6×** |
| `ArrayList.get(i)` | 643–727 | **387–421** | 27× → **16.5×** |
| `ArrayList.isEmpty()` | 557–624 | **312–328** | 25× → **14.5×** |
| `values().size()` | 15,345–15,873 | **1,012–1,032** | 780× → **51×** |
| `ArrayList.add` + `size` | 2,359–2,440 | **1,643–1,672** | |

CPU time is the load-independent instrument on this box (`probes/AlSizeOnly`,
20 M `ArrayList.size()` calls, user+sys, ABBA-interleaved, 16 samples per arm):

| arm | mean CPU |
| --- | ---: |
| `origin/dev` `ae2e1d9c8` | 10.71 s |
| after | **5.92 s** — 1.81× less CPU |

Every one of the 8 pairs agreed on the direction.

### The class this was filed against now passes

`io.netty.buffer.search.SearchProcessorTest`, run with the suite's own runner
and its own 120 s per-method timeout:

| arm | wall | result |
| --- | ---: | --- |
| HotSpot JDK 25 | 1.9 s | 15/15 |
| before | 159.3 s | 14/15 — `testUniqueLen64Substrings` `TimeoutException` |
| before | 152.8 s | 14/15 — same |
| after | 95.0 s | **15/15** |
| after | 105.4 s | **15/15** |

`AhoCorasicSearchProcessorFactory.buildTrie` makes 6.05 M ArrayList calls to
build its 256-entry-per-node trie, which is why this class and not another.

**Quote the cap, not the pass.** Those four runs were taken at load ~7. Repeated
at load ~19 the same binary is 14/15 again, and so is the old one — the fix
moves the class from ~1.3x OVER the 120 s per-method cap to ~0.8x of it, which
clears the cap on a reasonably quiet box and does not on a busy one. The margin
is real and it is not large; a reader who sees this class red under a full
parallel suite has not found a regression.

**No other class moved.** Eight netty classes run on both binaries back to back
— `BigEndianHeapByteBufTest` (414), `ByteBufUtilTest` (244 + 15 aborted),
`UnpooledTest` (41), `IntObjectHashMapTest` (35), `HttpRequestDecoderTest` (86),
`HttpResponseEncoderTest` (22), `PlatformDependentTest` (6 + 1),
`SearchProcessorTest` — report **identical** `ok`/`failed`/`aborted` counts.

## What the fix actually was — three things, in order

**1. The layout guard was re-derived per call.** `al_state` runs on every
`native_al_*` operation, and its receiver-layout guard called
`class_id_by_name("java/util/ArrayList")` plus `is_subclass` — two
class-manager read locks and a name hash over a 19-byte string — then a second
such pair for `java/util/Vector`. `native_al_size` reached `al_state` twice, so
one `size()` paid two of them. A flat `perf record -F 499` of a `size()`-only
loop put `class_id_by_name` + `find_unique_class_by_name` +
`classify_exact_name` at **15.9% of the whole run**, the largest family in that
profile, ahead of every dispatch symbol. Now `AlLayout`, memoized per `ClassId`
in the `AL_SLOTS` memo — and, like that memo, only ever filled with a RESOLVED
verdict, because while `ArrayList` is unloaded the old predicate answered a
lenient `true` that must not be frozen.

**2. The receiver's state was read twice.** `resync_values_view` opens with
`values_view_source`, which is itself an `al_state`; the caller then read the
same two fields again. On the non-view path nothing between them can move the
object, so the second read was pure duplication (`al_state_for_read`).

**3. `size()`/`isEmpty()` on a live view REBUILT the view.** A view's logical
size is by construction its source's size, so those two never needed the
elements at all — and the rebuild was O(n) with an allocation per call. One
virtual `size()` on the source replaces it. That single change is the 15.4× on
the `values().size` row.

**4. Then the carrier classes** (`MAP_VIEW_CARRIERS`), which is what made the
exact-class question meaningful in the first place.

### Why the REAL JDK class names were affordable here

`cratonvm/internal/ArrayListSubList` exists because a `subList` view has its own
native field layout and could not be laid over the real
`java.util.ArrayList$SubList`. A values view is different: it keeps its state in
ArrayList's own resolved `elementData`/`size` slots, which sit at absolute
indices 1 and 2 in the real-JDK layout — past the single `this$0` these classes
declare. Writing there is the undeclared-slot pattern `alloc_key_set_view_object`
already uses on the real `ConcurrentHashMap$KeySetView`: an undeclared slot
resolves to no field descriptor and is left untyped rather than mistyped.

The JDK's own bodies for those classes read `this$0`, which is null on a
CratonVM view, so every carrier joins `java/util/ArrayList` in BOTH force-native
gates (`force_native_over_real_jdk_bytecode` and its warm-path twin in
`vm_exec`) and carries the `native_al_*` family. `equals` and `hashCode` are
deliberately **not** registered on them: the JDK's views inherit
`AbstractCollection`'s identity semantics, and installing the LIST-contract
bodies would reinstate the `values().equals(anArrayList)` divergence the carrier
change removes.

## Behavioural cover

`probes/MapViewBehaviourProbe` — 194 assertions over `HashMap`,
`LinkedHashMap`, `TreeMap`, `Hashtable`, `ConcurrentHashMap`, `Properties` and
`IdentityHashMap`: size/isEmpty/contains/toArray/stream/iteration order/forEach,
write-through `remove`, `iterator().remove()`, `clear()`, the unsupported `add`,
and the copy constructors that read a view. Before and after diverge from
HotSpot **identically** — three pre-existing keySet/entrySet liveness rows,
now re-filed — so the carrier change introduced no behavioural difference at
all. `probes/ViewClassProbe`'s live-view section is likewise unchanged.

## What did NOT work, and is worth not repeating

Two further changes were made, measured, and kept only because they remove
provably redundant work — **neither moved CPU at all**:

* widening `record_object_ref_payload`'s one-entry per-thread memo to 8 ways
  (an `ArrayList` and its backing array are in different 4 KiB blocks, so
  `size()` alternated and evicted on every call — 3.3% of the profile);
* skipping the unmodifiable-wrapper receiver test for a receiver whose class
  already rules it out (1.0%, plus the 1.3% `class_name_rc` it drove).

Both symbols vanish from the profile afterwards. Total CPU: **0.4%, inside the
noise.** ~5.5% of attributed samples removed, ~0% of time. (A third attempt, a
class prefilter on the native-registry lookup, did pay — 4.7% — but only after
its first cut, which hashed the class name twice on a miss, measured 2.2%
*slower*.) See
[the VM-wide per-call page](../../../known-issues/vm-per-call-dispatch-cost-20260813.md)
for what the run is actually bound by, and treat a profile percentage on this VM
as a lead rather than a quantity.

## Repro

```bash
javac -d . probes/ViewClassProbe.java probes/AlCallCostProbe.java \
            probes/MapViewBehaviourProbe.java probes/AlSizeOnly.java

java ViewClassProbe > hs.txt                          # HotSpot oracle
cratonvm --java-home <jdk25> -cp . ViewClassProbe | diff hs.txt -
cratonvm --java-home <jdk25> -cp . AlCallCostProbe 3 200000
cratonvm --java-home <jdk25> -cp . MapViewBehaviourProbe

cd apps/netty-suite-runner
cratonvm --java-home <jdk25> --Xmx 1500m @common.args -Dcraton.batch=1 \
    CratonRunner io.netty.buffer.search.SearchProcessorTest
```
