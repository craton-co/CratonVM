# HIB-CV-05 — `NoSuchMethodError: java/util/ServiceLoader$Itr.hasNext()Z`

**Severity:** High — 2nd-largest CratonVM-only failure bucket (30 classes in the ~15% sample); also fires inside the EMF bootstrap, feeding HIB-CV-04's cascade.
**Status:** ✅ RESOLVED on `dev` — verified by re-test (fix `3a577182` "register ServiceLoader$Itr.hasNext/next in essential stream path" + de-dup `e5a5dab6`, both already merged into `dev`). Fix direction #2 below is what landed: the `ServiceLoader$Itr` Bridge natives are registered in the always-run essential stream path, so they resolve under `--java-home`. The synthetic iterator is still produced (direction #1 was not taken) but its `hasNext`/`next` now resolve.
**HotSpot:** not affected.

## Re-verification (2026-06-13, this worktree)

Ran `SLProbe`/`SLProbe2` under `--java-home "C:/Program Files/Java/jdk-25"` against the `dev` binary. All ServiceLoader/Stream iterator shapes resolve and match HotSpot — **no `ServiceLoader$Itr.hasNext()Z` NoSuchMethodError**:

| shape | HotSpot | CratonVM |
|-------|---------|----------|
| `Stream.of(List.of(...)).flatMap(List::stream).iterator()` | count=3 | count=3 |
| `ServiceLoader.load(Svc.class)` empty | hasNext=false | hasNext=false |
| for-each over `ServiceLoader.load(Svc.class)` (real provider) | count=1 | count=1 |
| `ServiceLoader.load(Svc.class, TCCL)` | count=1 | count=1 |
| `ServiceLoader.load(Svc.class).stream().map(Provider::get).iterator()` | count=1 | count=1 |

A separate **benign** `WARN NoSuchMethodError java/util/ServiceLoader$ProviderImpl.<init>(...)` is emitted on the `.stream()` path but does not affect the result (the provider is still constructed; `STREAMITER count=1`). Tracked separately if it ever turns fatal. The report below predates the verified fix landing in `dev`.

## Symptom

```
java.lang.NoSuchMethodError: java/util/ServiceLoader$Itr.hasNext()Z
(and ...Itr.next()Ljava/lang/Object;)
```

Fires whenever code iterates a `ServiceLoader` (`ServiceLoader.load(X).iterator()`) or a
`Stream.iterator()` that CratonVM routes through its synthetic iterator. Hibernate uses
`ServiceLoader` heavily during bootstrap (service discovery), so this appears on nearly every
EMF/SessionFactory bootstrap log line and fails 30+ test classes outright.

## Root cause

`java.util.ServiceLoader$Itr` **does not exist in JDK 25** (`javap java.util.ServiceLoader$Itr`
→ "class not found"; the real class uses `lookupIterator1`/`lookupIterator2` + inline iterators).
It is a **CratonVM-fabricated** class: the `ServiceLoader.iterator()` / `Stream.iterator()` bridges
(`native-builtins/src/phases_late.rs`, `alloc_concurrent_synthetic(ctx, "java/util/ServiceLoader$Itr", 2)`)
return a synthetic 2-field iterator. Its `hasNext`/`next` are registered as native overrides
(`native-builtins/src/streams.rs` `register_basestream_mode_overrides`, **Bridge** category,
`native_sl_itr_has_next` / `native_sl_itr_next`).

In **real-JDK mode** (`--java-home`), invoking `hasNext()` on that synthetic object resolves to
NoSuchMethodError — the registered Bridge native is **not consulted**. So the producer (the synthetic
`ServiceLoader$Itr`) survives but its consumer methods don't resolve. (This is the exact failure mode
the `streams.rs` comment for `SB-10` claims to have fixed for synthetic-JDK mode — it regresses under
`--java-home`.)

## Reproduction

```java
// CustomListProbe-style:
Stream.of(java.util.List.of("a","b","c")).flatMap(java.util.List::stream).iterator().hasNext();
// → NoSuchMethodError: java/util/ServiceLoader$Itr.hasNext()Z   (real-JDK mode)
```

## Fix direction

Two viable fixes:
1. **Preferred** — in real-JDK mode, do **not** intercept `ServiceLoader.iterator()` /
   `Stream.iterator()` to a synthetic `ServiceLoader$Itr`; let the real `ServiceLoader` /
   `ReferencePipeline` bytecode run (`stream().count()` already does and works). The synthetic
   iterator is a synthetic-JDK artifact that has no place under `--java-home`.
2. Make the interpreter's `invoke_or_native` method resolution consult the Bridge native registry
   for the fabricated `java/util/ServiceLoader$Itr` class on a bytecode-method miss (it claims to
   "try native first" — verify why the Bridge entry is skipped for this synthetic class in real-JDK
   mode; likely the no-stubs `drop_synthetic_stubs` filter still drops it, or a later `SyntheticStub`
   re-registration shadows + then is dropped).

Fixing this should clear the 30 direct failures and remove a bootstrap-time exception source behind
HIB-CV-04.
