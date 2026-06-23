# SBR-12 — `getClass()` returns an interface/abstract/internal class for synthetic objects

**Status:** ✅ **FIXED** (branch `fix/getclass-concrete-synthetic-class`) — verified byte-identical to HotSpot jdk-25 on the affected probes.
**Approach:** option (b) — a central, memoised `getClass()` *display* substitution (no backing/storage rework).

## Symptom

Several CratonVM synthetic factories stamped their result with the *interface*
or *abstract* type it stands in for, or with a private craton-internal name —
impossible on a real JVM, where an instance's runtime class is always concrete:

| call | CratonVM (before) | HotSpot |
|---|---|---|
| `IntStream.rangeClosed(1,4).getClass()` | `java.util.stream.IntStream` (interface) | `java.util.stream.IntPipeline$Head` |
| `List.of(a,b).getClass()` | `cratonvm.internal.UnmodifiableList` | `java.util.ImmutableCollections$List12` |
| `FileSystems.getDefault().getClass()` | `java.nio.file.FileSystem` (abstract) | `sun.nio.fs.WindowsFileSystem` |
| `jarUrl.openConnection().getClass()` | `java.net.JarURLConnection` (abstract) | `sun.net.www.protocol.jar.JarURLConnection` |

## Fix (2026-06-23)

`native_object_get_class` (and the default `Object.toString` name path) now
re-map a small, fixed set of synthetic interface/abstract/internal *stamps* to
the concrete, JDK-plausible class HotSpot reports. Key facts that make this safe
and contained:

* **Dispatch is decoupled from the stamp.** Native method dispatch keys on the
  *resolved declaring class* (the interface/abstract type), not the receiver's
  runtime stamp (`vm/src/runtime/interpreter.rs`). Re-reporting the class
  therefore changes only the Java-visible `Class` mirror — never storage,
  dispatch, or GC layout. Object identity is preserved (e.g. the
  `FileSystems.getDefault()` singleton `==` still holds).
* **Memoised.** A thread-local `ClassId → strategy` cache keeps the hot
  `getClass()`/`toString()` paths allocation-free after warm-up (the steady
  state is one map lookup).
* **Size-discriminated collections.** The `java.util` immutable-collection
  family is resolved per object via the receiver's own `size()`:
  `List.of()`→`ListN`, size 1–2→`List12`, ≥3→`ListN`; `Set` mirrors `List`;
  `Map.of(k,v)`→`Map1`, else `MapN` — matching HotSpot exactly.

Covered stamps: the `Stream`/`IntStream`/`LongStream`/`DoubleStream` interfaces,
`java.net.JarURLConnection`/`HttpURLConnection`, the `java.nio.file`
`FileSystem`/`FileSystemProvider`/`Path` family, and the
`cratonvm.internal.Unmodifiable{List,Set,Map}` backing classes.

Regression test: `vm/tests/getclass_concrete_class.rs` (self-contained — embeds
and compiles its probe to a temp dir). Manual parity also confirmed via
`IntSortProbe2`, `FSEq`, `LOfHash2`, `KUrlProbe`.

## Follow-up (2026-06-23) — immutable vs unmodifiable split

The shared-stamp residual is now resolved **without** a backing-class/dispatch
rework. `List.of`/`Set.of`/`Map.of`/`copyOf` and `Collections.unmodifiable*`
keep the same `cratonvm/internal/Unmodifiable*` stamp (so all dispatch guards are
untouched), but the immutable factories now set a marker field (slot 1,
`UNMOD_FIELD_IMMUTABLE`, via `alloc_immutable_wrapper`). `getClass()` reads it
per object:

* marker set → `java.util.ImmutableCollections$*` (size-discriminated, as before)
* marker unset → `java.util.Collections$Unmodifiable*`; for lists the backing's
  `RandomAccess`-ness (checked via `is_subclass`) selects
  `UnmodifiableRandomAccessList` vs `UnmodifiableList`, matching HotSpot exactly.
  `Collections$UnmodifiableCollection` is also now mapped.

Verified byte-identical to HotSpot jdk-25 for `List.of`/`copyOf`/
`unmodifiableList(ArrayList|LinkedList)`/`unmodifiableSet`/`unmodifiableMap`/
`unmodifiableCollection` (regression test extended).

## Follow-up (2026-06-23) — real-JDK singletons + interface-native delegation

`Collections.singletonList`/`singleton`/`singletonMap` previously returned
synthetic `ArrayList`/`HashSet`/`HashMap` (wrong class **and** wrongly mutable).
Per the "real JDK by default" principle they now return **real**
`Collections$Singleton{List,,Map}` objects (`alloc_real_jdk` allocates the real
class with its natural field count and sets `element` / `k`,`v` by name; a
synthetic fallback remains if the class is unavailable). Their `get`/`iterator`/
`contains`/`toString`/mutator-throws run real JDK bytecode via the receiver-walk.

The one obstacle was that `size`/`isEmpty` are registered on the
`Collection`/`List`/`Set`/`Map` **interfaces** as a fallback and bailed to a
`0`/empty sentinel for any non-ArrayList/HashSet/HashMap layout — so a real
singleton reported `size()==0`. Fixed generally: those 5 natives now
**delegate to the receiver's own bytecode** (`invoke_special`, whose exact
per-class native lookup finds nothing for these real classes → runs real
`Code`, no recursion) when the receiver is a real JDK collection
(`instanceof Collection`/`Map`) that CratonVM has **no** synthetic backing for
(`!is_synthetic_backed_collection`). CratonVM's own synthetic collections keep
their fast path; genuinely-foreign receivers (`Charset`/`String` funnelled via a
misresolved vtable) keep the safe sentinel. This is a real-JDK-completeness win
beyond singletons — e.g. `((Collection) linkedList).size()` and
`Collections.empty*` now run correct real bytecode too.

Verified byte-identical to HotSpot jdk-25 for the whole singleton family
(class + `size`/`get`/`isEmpty`/`contains`/iteration/`toString`/`keySet`/
`values`/`entrySet`/`equals`/`hashCode`/mutation-throws) and for a normal vs
foreign collection smoke (ArrayList/HashSet/HashMap/LinkedList/emptyList/
emptyMap). native-collections 69 unit tests + getclass/sublist/cluster_b/
intrinsic_diff integration tests all green.

## Residuals (knowingly deferred — still concrete, the floor is met)

* *Views* of immutable collections (`List.of(..).subList(..)`,
  `Map.of(..).keySet()`, …) report the `Collections$Unmodifiable*` family rather
  than HotSpot's `ImmutableCollections$SubList` etc. — they reuse the plain
  unmodifiable wrapper and carry no marker. All concrete; rare in practice.
* An *intermediate* stream op (`.map(..)`) reports `IntPipeline$Head` rather than
  HotSpot's `IntPipeline$4`. The exact name is a JDK anonymous-class index
  (`$1`/`$2`/`$3`/`$4`/`$10`…, plus `SortedOps$OfInt`) — a declaration-order
  artifact that varies across JDK builds and that no user code inspects. Matching
  it would require threading per-op tags through every eager-stream native and
  hard-coding a fragile version-specific table; deliberately not done.

All residuals report a concrete, JDK-plausible class — the original bug
(interface/abstract/internal `getClass()`) is gone in every case.
