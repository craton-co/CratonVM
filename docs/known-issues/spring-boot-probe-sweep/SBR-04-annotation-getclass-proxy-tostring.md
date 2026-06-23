# SBR-04 — Annotation `getClass()` returns the annotation type (not `$ProxyN`) + `toString` format diverges

**Status:** ◐ Open — overlaps prior **SB-09** (foundation fixed, full re-arch gated).
**Recommendation:** FIX (incremental) — two concrete, observable gaps below.
**Binary:** `cvsbfull.exe` (dev `df11ac00`) vs HotSpot `jdk-25`.

## Affected probes

`DAnnCore`, `DAnnWalk` (annotation portion). (`DTags` shares the file but its
divergence is method-order — see SBR-05.)

## Symptom — two distinct gaps

`DAnnCore` prints, for each annotation, `toString` / `getClass` / `annotationType`:

```
CratonVM:
  [1] toString=@java.lang.annotation.Retention(value=RUNTIME)   getClass=java.lang.annotation.Retention   annotationType=...Retention
  [2] toString=@java.lang.annotation.Target(value=[ANNOTATION_TYPE]) getClass=java.lang.annotation.Target  annotationType=...Target
HotSpot:
  [1] toString=@java.lang.annotation.Retention(RUNTIME)         getClass=jdk.proxy1.$Proxy0               annotationType=...Retention
  [2] toString=@java.lang.annotation.Target({ANNOTATION_TYPE})  getClass=jdk.proxy1.$Proxy2               annotationType=...Target
```

1. **`getClass()`** — CratonVM returns the **annotation interface itself**
   (`java.lang.annotation.Retention`); HotSpot returns the **proxy** runtime class
   (`jdk.proxy1.$Proxy0`). `annotationType()` is correct on both.
2. **`toString()` format** — CratonVM uses the old JDK style:
   - single-member: `Retention(value=RUNTIME)` vs HotSpot `Retention(RUNTIME)`
     (JDK ≥ 9 omits `value=` for the sole `value` member).
   - array member: `Target(value=[ANNOTATION_TYPE])` vs HotSpot `Target({ANNOTATION_TYPE})`
     (braces, not brackets; no `value=`).

`DAnnWalk` confirms (1): every `a.getClass()` it prints is the annotation type
under CratonVM vs `jdk.proxyN.$ProxyM` under HotSpot.

## Root cause (hypothesis)

CratonVM models annotation instances as direct instances of the annotation
interface rather than as `Proxy`/`AnnotationInvocationHandler`-backed `$ProxyN`
objects (the SB-09 "full re-arch gated" note). The `toString` formatter is a
separate, older implementation that predates the JDK 9 single-`value` / array
brace formatting.

## Repro

```bash
cd C:/craton/CratonVM/apps/spring-boot/buildSrc
CP="runner;$(cat test-classpath.txt)"
"C:/craton/CratonVM-sbfull/target/release/cvsbfull.exe" --java-home "C:/Program Files/Java/jdk-25" -cp "$CP" DAnnCore
"C:/Program Files/Java/jdk-25/bin/java.exe" -cp "$CP" DAnnCore
```

## Impact

`getClass()` divergence breaks code that special-cases proxy classes or compares
`annotation.getClass() != annotationType()`. The `toString` divergence breaks
golden-output and annotation-dump assertions. The `toString` fix is small and
independent of the proxy re-arch — worth landing on its own.
