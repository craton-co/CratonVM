# spring-bug-06: `MergedAnnotationsTests` HANGS (infinite loop in annotation merge)

> **UPDATE 2026-06-20 (deep root-cause; supersedes the JIT-codegen guess):** With the fam6
> synthesis fixes (branch `fix/bug06-fam6-repeatable-merge`) the class completes 172/178 under
> `--nojit` but TIMEOUTs (EXIT 124/127) under JIT. The hang is `ReferencePipeline.toArray()`
> infinite self-recursion. **It is NOT a JIT-codegen bug** — it reproduces identically under
> `--nojit` / `CRATONVM_DISABLE_JIT=1` / `CRATONVM_JIT_VIRTUAL_TIERUP=0` / `CRATONVM_DISABLE_INTRINSICS=1`.
> The earlier "JIT-only" framing was wrong; JIT just makes the real suite *reach* the trigger.
>
> **Minimal reproducer** (`spring-suite/probe/ISn.java`, hangs in seconds):
> ```java
> for (int k=0;k<N;k++) (k%3==0?OptionalInt.empty():OptionalInt.of(k%7)).stream()
>      .mapToObj(i->"["+i+"]").collect(Collectors.joining());
> ```
> Completes for N≤1000, HANGS for N≥2000 — a count-triggered cache/warmup promotion at ~1000–2000.
> Originates from JUnit's `JupiterTestDescriptor.getLegacyReportingName()` = `indexes.mapToObj(..).collect(joining())`.
>
> **Mechanism (instrumented `invoke_or_native` + `invoke_on_class_shared_inner` + cached dispatch):**
> On CratonVM the stream chain is synthetic interface-classed stubs (`OptionalInt.stream()` → an object
> whose class IS `java.util.stream.IntStream`; `mapToObj` → class `java.util.stream.Stream`). Calling
> `mapToObj().toArray()` directly works (a `toArray` native handles the stub — `ISt.java` loops 3000× clean).
> But after ~1000 iters the `collect(joining())` path switches the chain to **real JDK bytecode**
> (a native→bytecode promotion, the documented `interpreter.rs:~14690` "interface-bridge dispatched first
> call, cached bytecode later" class), producing a **real `IntPipeline$1`**. `collect` then calls no-arg
> `IntPipeline$1.toArray()`, which resolves correctly to `ReferencePipeline.toArray()` (real bytecode,
> `hasCode=true`, non-synthetic). That bytecode's `invokevirtual #219` (= `ReferencePipeline.toArray:(IntFunction)`,
> verified via `javap -v`) is then dispatched as the **no-arg `toArray()`** again — `toArray(IntFunction)`
> and the cached path are NEVER reached. So the inherited `ReferencePipeline.toArray()` frame resolves its
> own constant-pool method-ref against the WRONG class (the receiver subclass `IntPipeline$1`, not the
> declaring class `ReferencePipeline`) → `toArray()`→`toArray()` forever. Normal inherited calls use the
> fast/cached path (CP-correct), which masks this; it surfaces only for the slow/uncached inherited
> dispatch on a receiver whose runtime class ≠ the inherited method's declaring class.
>
> **Same bug as [[bug06-fam6-annotation-synthesis-mergedannotation]]'s "~2 GB OOM".** OPEN — fix is in the
> slow inherited-method dispatch (frame constant-pool class), a core path; needs careful work + full
> regression. A safer interim mitigation: prevent the stream-factory natives (`OptionalInt.stream`,
> `mapToObj`) from promoting to real JDK bytecode so the chain stays on the working synthetic-stub path.

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
