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

## Residuals (knowingly deferred — still concrete, the floor is met)

* `List.of`/`copyOf` and `Collections.unmodifiableList` share the
  `cratonvm/internal/UnmodifiableList` backing stamp, so a `unmodifiableList`
  wrapper now reports the `ImmutableCollections$List{12,N}` family rather than
  HotSpot's `Collections$UnmodifiableRandomAccessList`. An exact fix needs the
  backing-stamp split (separate immutable vs unmodifiable classes + their
  dispatch guards in `native-collections`).
* An *intermediate* stream op (`.map(..)`) reports `IntPipeline$Head` rather
  than HotSpot's `IntPipeline$4`; CratonVM's synthetic streams are eager, so
  there is no faithful op-chain subtype to report.

Both residuals report a concrete, JDK-plausible class — the original bug
(interface/abstract/internal `getClass()`) is gone in every case.
