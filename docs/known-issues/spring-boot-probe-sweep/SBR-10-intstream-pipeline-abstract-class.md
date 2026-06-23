# SBR-10 — `IntStream` pipeline objects report abstract `IntStream` class

**Status:** 🟠 Open — object-identity cluster (deferred).
**Recommendation:** FIX — concrete-type gap in the stream implementation.

> Investigated 2026-06-22: same cluster as SBR-08/09/11/13 — stream stages report
> the `IntStream` interface instead of the `IntPipeline$*` concrete classes.
> Stream **results** are correct (IntSortProbe/IntSortProbe3 match); only the
> stage class identity diverges. Subsystem fix.
**Binary:** `cvsbfull.exe` (dev `df11ac00`) vs HotSpot `jdk-25`.

## Affected probe

`IntSortProbe2`.

## Symptom

```
                       CratonVM                         HotSpot
rangeClosed class      java.util.stream.IntStream       java.util.stream.IntPipeline$Head
rangeClosed.map class  java.util.stream.IntStream       java.util.stream.IntPipeline$4
```

`IntStream.rangeClosed(...)` and `.map(...)` return objects whose `getClass()` is
the **interface** `java.util.stream.IntStream` under CratonVM, but the concrete
pipeline classes (`IntPipeline$Head`, `IntPipeline$4`) under HotSpot.

## Root cause (hypothesis)

CratonVM's stream factory returns objects typed as the `IntStream` interface
rather than instances of the `java.util.stream.IntPipeline` hierarchy — its
streams are a CratonVM-internal implementation surfaced under the public
interface name. (Note: `IntSortProbe`/`IntSortProbe3` in the same sweep matched
HotSpot, so the **results** are correct; only the runtime **class identity** of
the stream stages diverges.)

## Repro

```bash
cd C:/craton/CratonVM/apps/spring-boot/buildSrc
CP="runner;$(cat test-classpath.txt)"
"C:/craton/CratonVM-sbfull/target/release/cvsbfull.exe" --java-home "C:/Program Files/Java/jdk-25" -cp "$CP" IntSortProbe2
"C:/Program Files/Java/jdk-25/bin/java.exe" -cp "$CP" IntSortProbe2
```

## Impact

Type-identity only; stream results are correct. Affects reflection/`instanceof`
on `IntPipeline` internals (rare). Low urgency.
