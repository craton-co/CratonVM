# Bug 02 — `Spliterators.emptySpliterator()` returns the abstract base class → NoSuchMethodError

**Severity:** High — crashes `clients.admin`, `clients.producer`,
`clients.producer.internals` (and any code that drives an empty spliterator).
Reproduces under `--nojit` (interpreter), so it is **not** JIT-related.
HotSpot runs all three packages clean.

**Symptom:**
```
NoSuchMethodError: java/util/Spliterators$EmptySpliterator.tryAdvance(Ljava/util/function/Consumer;)Z
linkage error: no such method: ...EmptySpliterator.tryAdvance(Ljava/util/function/Consumer;)Z
```

**Minimal repro:**
```java
Object o = java.util.Spliterators.emptySpliterator();
System.out.println(o.getClass().getName());
```
- HotSpot: `java.util.Spliterators$EmptySpliterator$OfRef`
- CratonVM: `java.util.Spliterators$EmptySpliterator`  ← **wrong (abstract base)**

`Spliterators.emptySpliterator()` reads `EmptySpliterator$OfRef.EMPTY_SPLITERATOR`
(a `getstatic`, initialised in `OfRef.<clinit>` as `new OfRef()`). CratonVM ends
up with an instance whose class is the **abstract** `EmptySpliterator`, not the
concrete `OfRef`. The bridge method `tryAdvance(Consumer)Z` lives on `OfRef`, so a
call on the wrong (abstract) receiver fails to resolve → NSME.

## Diagnosis so far
- Boot classes are loaded from the real JDK `java.base.jmod` (verified) — the
  bytecode is identical to HotSpot's.
- `Class.forName("...$OfRef")` resolves correctly (super = `EmptySpliterator`),
  and reflective `OfRef.newInstance()` produces the **correct** `OfRef`.
- Only the value returned by `emptySpliterator()` (i.e. the `OfRef.<clinit>`
  `new OfRef` → static field) has the wrong class.
- Numerous structural Java clones (double-nesting; nested-extends-enclosing;
  `new self` in own `<clinit>`; 3 type params + 4 sibling subclasses) all behave
  **correctly** on CratonVM — the defect is specific to this real boot class.
- The synthetic `Spliterators.emptySpliterator` native (`phases_late.rs::register_p69_spliterator`)
  is compiled out of the real-JDK build, so the real bytecode path is the one
  running. The `new` instruction trace (`CRATONVM_DBG_NEW`) is being used to pin
  whether `OfRef.<clinit>`'s `new` resolves to the wrong class id, or the class is
  substituted at load/getstatic time. (investigation continuing)

## Status
- [x] Reproduced minimally; confirmed CratonVM-only (HotSpot correct).
- [ ] Root cause pinned / fixed (open).
