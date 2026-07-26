<!-- One file per CratonVM-unique crash/hang/correctness root cause. -->
# toArray-recursion: `ReferencePipeline.toArray(IntFunction)` native re-entered no-arg `toArray()` → `StackOverflowError` (was the `MergedAnnotations` hang / bug-06 fam6 "OOM")

| | |
|---|---|
| **Category** | VM-CORRECTNESS (stream terminal dispatch) |
| **Module** | native-collections (stream natives) |
| **Symptom** | `java.lang.StackOverflowError` with a stack that is only `java.util.stream.ReferencePipeline.toArray(ReferencePipeline.java:658)` repeated ~1024 deep |
| **CratonVM HEAD** | reproduced on dev `697134f8` |
| **Status** | ✅ **FIXED** on branch `fix/springsuite-0620-residuals`, commit `8795b88d` (→ merged to dev) |

## Why it mattered
This single bug **blocked the entire JUnit-Platform Spring suite** under CratonVM. The
launcher's `TestPlan.from(...)` / `getLegacyReportingName()` path calls `Stream.collect`/
`toArray` on **real** JDK pipelines, so *every* test class died with `StackOverflowError`
during launch — `RESULT` lines were unreachable. It is also the same root cause as two
previously-open items:

- **`spring-bug-06` — `MergedAnnotations` hang** (`MergedAnnotations.stream()` materialises
  via `toArray()`).
- **bug-06 fam6 — `AnnotationUtilsTests` "~2 GB OOM"** (the runaway recursion exhausts the
  stack/heap rather than truly OOMing).

## Root cause
A real JDK `ReferencePipeline`'s no-arg `toArray()` (`ReferencePipeline.java:658`) is:

```java
public final Object[] toArray() { return toArray(Object[]::new); }   // delegates to the generator overload
```

`native-collections` materialises a **real** (non-synthetic) pipeline inside
`stream_elements()` by calling that no-arg `toArray()` via `invoke_virtual`. But it had **also**
registered `native_stream_to_array_gen` on the concrete class
`java/util/stream/ReferencePipeline.toArray(IntFunction)`. So the real no-arg bytecode bounced
into our native, which re-entered `stream_elements()`, which called no-arg `toArray()` again:

```
stream_elements()
  → toArray()            [real JDK bytecode, line 658]
    → toArray(IntFunction) [our native_stream_to_array_gen]
      → stream_elements()
        → toArray() → …                        ⇒ StackOverflowError
```

The native has no Java stack frame, so the trace shows only line 658 repeating — which is why
it looked like a no-arg method calling itself.

Synthetic CratonVM streams were never affected: their class name is `java/util/stream/Stream`,
so `stream_elements()` reads their backing array (slot 0) directly and never calls `toArray()`.
That is also why isolated `Stream.of(...).toArray()` / `Arrays.stream(...).toArray()` repros
*passed* — those produce synthetic streams. Only **real** pipelines (e.g. from JDK-internal
`IntStream.mapToObj(...)`, `MergedAnnotations.stream()`, the JUnit launcher) recursed.

## Fix
Drop the `ReferencePipeline.toArray(IntFunction)` native override (in
`../../../../native-collections/src/lib.rs`, `register_stream_natives`). Real pipelines now run the real
JDK `toArray(IntFunction)` bytecode end-to-end — exactly what `stream_elements()`' real-pipeline
branch already relies on. Synthetic streams keep the **interface-level**
(`java/util/stream/Stream`) generator native, which preserves the Kafka `Utils.enumOptions`
typed-array case (the reason the generator native was added in the first place).

## Verified (build `cratonvm-spring0620` off dev `697134f8`, JDK 25)
- `MergedAnnotationsTests` **174/178** (was: hang / `StackOverflowError`); the 4 residual
  failures are annotation-**synthesis** mismatches (bug-06 fam6 / `spring-bug-01`), a separate
  open bug — the *hang* is gone.
- `AnnotationUtilsTests` **72/72** (was: "~2 GB OOM").
- `AnnotatedElementUtilsTests` **82/82**.
- `PooledDataBufferTests` **10/10**, `LeakAwareDataBufferFactoryTests` **2/2**.
- Real + synthetic `Stream.toArray()` / `toArray(gen)` / `collect` / `toList` / `count` all
  match HotSpot; **zero** `StackOverflowError` in the launcher path.

## Related / still-open
- `spring-bug-06` (MergedAnnotations) **hang** is resolved by this fix; the synthesis-value
  mismatches (bug-06 fam6 / `spring-bug-01`) remain open.
- A sibling latent defect lives in `../../../../native-builtins/src/phases_early.rs`: `toList()` is
  overridden on `ReferencePipeline` / `ReferencePipeline$Head` and reads slot 0 as a backing
  array, which is wrong for a **real** pipeline (returns an empty/garbage list rather than
  recursing). Not exercised by the suite paths fixed here; noted for follow-up.
