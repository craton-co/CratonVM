# spring-bug-06: `MergedAnnotationsTests` HANGS (infinite loop in annotation merge)

> **UPDATE 2026-06-20:** Re-characterised. The hang is **JIT-ONLY**. Under `--nojit` the class
> now completes (172/178 after the fam6 synthesis fixes on branch `fix/bug06-fam6-repeatable-merge`).
> Under JIT it still TIMEOUTs (EXIT 124). Captured root cause: `java.util.stream.ReferencePipeline.toArray()`
> infinite-recurses into itself — the `invokevirtual toArray(IntFunction)[Object` at pc6 mis-dispatches
> to the no-arg `toArray()[Object` overload (a JIT virtual-overload-resolution bug), accompanied by a
> GC-guard'd OOB slot-probe (`index=1 num_slots=1`) on a `java/util/stream/Stream` object. **This is the
> SAME bug as the [[bug06-fam6-annotation-synthesis-mergedannotation]] "~2 GB OOM"** — they unify. It is
> NOT the annotation-proxy `equals`/`hashCode` issue the original hypothesis (below) blamed — that turned
> out to be a real but SEPARATE bug, now FIXED. The hang is receiver-specific: plain `Stream.of(..).toArray()`
> does NOT reproduce it (works under JIT and interpreter). NEXT: localize the JIT invokevirtual overload
> resolution that picks the no-arg `toArray()` for the Spring-suite stream receiver (release backtraces are
> unsymbolized — needs a debug build or targeted JIT-dispatch instrumentation). OPEN (JIT codegen).

| | |
|---|---|
| **Category** | **VM-HANG** (first hang found in the suite) |
| **Module** | spring-core |
| **Test class** | `org.springframework.core.annotation.MergedAnnotationsTests` |
| **CratonVM** | TIMEOUT — `BEGIN` printed, never emits `RESULT` (killed at 90s) |
| **HotSpot JDK 25** | OK (completes well under 70s) |
| **CratonVM HEAD** | c5644da4 (dev) |
| **Status** | OPEN |
| **Suggested owner** | **me** |

## Symptom
CratonVM prints `BEGIN org.springframework.core.annotation.MergedAnnotationsTests` and then never
returns — no `RESULT`, no crash, pegged on CPU until the external 90s watchdog kills it. HotSpot
runs the same class to completion. Classic CratonVM infinite loop / non-termination.

## Reproduce
```bash
VM=C:/craton/CratonVM/target/release/cratonvm.exe
JDK='C:\Program Files\Eclipse Adoptium\jdk-25.0.2.10-hotspot'
H=C:/craton/CratonVM-spring/spring-suite
CP="$H;$(tr -d '\r' < C:/craton/cratonvm/apps/spring-framework/spring-core/build/cratonvm-testcp.txt)"
# hangs (kill after ~30s):
"$VM" --java-home "$JDK" -cp "$CP" KRun org.springframework.core.annotation.MergedAnnotationsTests
# HotSpot completes:
"$JDK\bin\java.exe" -cp "$CP" KRun org.springframework.core.annotation.MergedAnnotationsTests
```
Next step: run with `--stack-dump-on-timeout 1` (or attach) to capture WHERE it spins — almost
certainly inside `AnnotationTypeMapping` / `MirrorSets` / `MergedAnnotation` resolution.

## Root cause — captured (`--stack-dump-on-timeout 30`)
The hang is **during JUnit test discovery, not the test body**. Stable depth-25 stack (a LOOP,
not growing recursion), top frames:
```
org/junit/platform/commons/util/AnnotationUtils.findAnnotation     (pc 13↔10, oscillating)
org/junit/platform/commons/util/ReflectionUtils.streamMethods      (pc 39↔34)
org/junit/platform/commons/util/ReflectionUtils.findMethods
org/junit/jupiter/.../LifecycleMethodUtils.findMethodsAndCheckVoidReturnType
org/junit/jupiter/.../ClassBasedTestDescriptor$LifecycleMethods.<init>   (discovery)
```
JUnit is scanning `MergedAnnotationsTests`' methods + annotations and **never terminates**.
`MergedAnnotationsTests` is unusually annotation-dense (many meta/repeatable/nested annotation
fixtures). The most likely cause: CratonVM's **annotation-proxy `equals`/`hashCode`** (or
`annotationType()`/class-hierarchy reflection) is inconsistent, so JUnit's `findAnnotation`
cycle-detection `visited` set never recognises an already-seen meta-annotation → infinite
meta-annotation walk. Alternatively `Class.getMethods()`/`getInterfaces()` returns a cyclic or
duplicate-laden result that makes `streamMethods` loop. **This is a symptom of [[spring-bug-01]]**
(annotation-proxy identity/correctness) — re-test after bug-01's `equals`/`hashCode` is fixed.

## Status update
Hang reproduced + located. Fix tracked under [[spring-bug-01]] (annotation proxy). Will re-verify
this class in isolation after bug-01 lands.

## Notes
Fixing [[spring-bug-01]] may resolve this hang as a side effect — re-test after bug-01 lands.
First hang in the suite; expect more in spring-context / spring-test (AOP proxies, scheduling).
