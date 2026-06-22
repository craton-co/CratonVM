# SC hangs: MergedAnnotationsTests + CharSequenceEncoderTests

Static-analysis investigation (no VM/cargo/gradle executed — a suite was running).

- CratonVM Rust source: `C:/craton/CratonVM-spring0621`
- Spring test source: `C:/craton/cratonvm/apps/spring-framework/spring-core/src/test/java`
- Run log: `C:/craton/CratonVM-spring0621/spring-suite/full-run/spring-core.log`
  - Line 317: `TIMEOUT org.springframework.core.annotation.MergedAnnotationsTests`
  - Line 333: `TIMEOUT org.springframework.core.codec.CharSequenceEncoderTests`
  - (Line 334: `TIMEOUT ...ResourceRegionEncoderTests` — sibling codec class, not assigned, see note.)

Hang-stack capture available in this tree: `CRATONVM_DBG_HANGWALK=<secs>` — native-stack-walk watchdog at `C:/craton/CratonVM-spring0621/vm/src/runtime/stwhang_watch.rs:1,21`. Set it to a value shorter than the suite timeout to dump the native stack of the wedged thread.

---

## 1. org.springframework.core.annotation.MergedAnnotationsTests

### Symptom
Whole test class TIMEOUTs (infinite loop / runaway recursion). Passes on HotSpot/JDK25. ~3000-line test that introspects annotations heavily.

### What the test does
- Drives `MergedAnnotations.from(...).get(...) / .stream() / .collect(...)` across many strategies (DIRECT, SUPERCLASS, TYPE_HIERARCHY, INHERITED_ANNOTATIONS).
- 44 stream/`map`/`collect`/`toArray` occurrences in the test file (e.g. `MergedAnnotations.search(...).from(X).map(...)`, `.stream().map(...).collect(...)`).
- Contains a deliberate **meta-annotation 3-cycle**: `@MetaCycle1` → `@MetaCycle3` → `@MetaCycle2` → `@MetaCycle1`
  (`MergedAnnotationsTests.java:2838-2851`), exercised by `getWithInheritedAnnotationsFromMetaCycleAnnotatedClassWithMissingTargetMetaAnnotation` (line 477) and `getDirectFromClassWithMetaCycleAnnotatedClassWithMissingTargetMetaAnnotation` (line 1018).
- Heavy `@Repeatable` container, `@Inherited`, and annotation `synthesize()`/proxy use.

### Hang hypothesis (best-supported): JIT/native `Stream.toArray` infinite recursion on a real `ReferencePipeline`
`MergedAnnotations.stream()` returns a **real JDK `ReferencePipeline`** (not a CratonVM synthetic stream). When native code needs its elements it calls `stream_elements`:

- `native-collections/src/lib.rs:9468` `stream_elements` — for a non-synthetic stream it materialises via real `toArray()`:
  `native-collections/src/lib.rs:9508` `ctx.invoke_virtual(stream, "toArray", "()[Ljava/lang/Object;", &[])`.
- Real `ReferencePipeline.toArray()` (JDK bytecode) delegates to `toArray(IntFunction)` = the generator overload.
- CratonVM registers `toArray(IntFunction)` → `native_stream_to_array_gen` on the **interface** `java/util/stream/Stream` (`c` set at `native-collections/src/lib.rs:9585`; registration `:9703-9708`). `native_stream_to_array_gen` (`:10678`) calls `stream_elements` again.

The code is **explicitly aware** of this exact bug. The comment at `native-collections/src/lib.rs:9709-9729` names it:
> "stream_elements → toArray() [real bytecode] → toArray(IntFunction) [our native] → stream_elements → toArray() → … ⇒ StackOverflowError. This was the open `MergedAnnotations.stream()` / bug-06 fam6 '~2 GB OOM' and it blocked the entire JUnit-Platform Spring suite."

The mitigation is purely "we deliberately do NOT override `toArray(IntFunction)` on the concrete `ReferencePipeline` class" — i.e. it relies on CratonVM's method dispatch **never** letting the interface-level `java/util/stream/Stream.toArray(IntFunction)` native shadow a real concrete `ReferencePipeline` instance's bytecode. That guarantee is fragile: CratonVM has a documented history of native shadows leaking onto inherited/interface methods of real bytecode objects (see MEMORY: "invoke-cache native-shadow miss for inherited methods", "per-class natives shadowed real JDK bytecode", "Object.toString intrinsic still shadows in some paths"). If the interface-level native is dispatched for a `ReferencePipeline` here, the documented infinite bounce reappears. The fam6 history pins this loop to `MergedAnnotations.stream()` specifically — the same API this test hammers.

Recursion site (the bounce): `native-collections/src/lib.rs:9508` ↔ `:10687` (`native_stream_to_array_gen` → `stream_elements`), guarded only by the non-registration documented at `:9709-9729`.

### Hypotheses considered and rejected
- **Meta-cycle parser recursion** (proposed by a sub-agent: `convert_annotation`/`convert_element_value` at `vm/src/vm/vm_exec.rs:6127,6146,6207-6217`): **rejected.** Those functions only recurse into *element values nested inside one annotation's own bytecode* (`@Outer(inner=@Inner)`). The test's meta-cycle is a cycle of annotation **types** (annotations on annotation declarations), which lives in each type's `RuntimeVisibleAnnotations` and is not reachable as element-value nesting in a valid classfile. Parsing one class's annotations does not follow into the referenced annotation type's classfile, so no cycle is traversed here. Spring's own `AnnotationTypeMappings`/`AnnotationsScanner` carry the visited-sets for the type-level meta-walk, and CratonVM's `repeatable_container_desc` (`native-builtins/src/lang_class.rs:8730-8740`) reads only one meta level. Lower confidence than toArray.

### How to confirm
1. Minimal repro (run with JIT on, then `--nojit`, to localise to JIT vs native dispatch):
   ```java
   import org.springframework.core.annotation.*;
   public class StreamToArrayRepro {
     public static void main(String[] a) {
       Object[] r = MergedAnnotations.from(java.util.ArrayList.class,
           MergedAnnotations.SearchStrategy.TYPE_HIERARCHY)
           .stream().toArray();           // forces ReferencePipeline.toArray()
       System.out.println(r.length);
     }
   }
   ```
   Hang here = the bounce. Also try `.stream().map(x->x).toArray(Object[]::new)` and `.collect(...)`.
2. Capture the hang stack: `CRATONVM_DBG_HANGWALK=20` and look for repeated `native_stream_to_array_gen` / `stream_elements` / `toArray` frames (recursion) vs. a parked/wait frame (would point elsewhere).
3. Bisect with the recipe in MEMORY (`BISECT_ONLY` prefix / `BISECT_SKIP=Stream.toArray`) to confirm the loop is the stream terminal.

### Suspected subsystem
native-collections stream materialisation (`stream_elements` / `native_stream_to_array_gen`) + method-dispatch native-shadow scoping (whether an interface-level native shadows a concrete `ReferencePipeline`). Secondary: JIT (fam6 was tagged a "JIT toArray() infinite-recursion").

### Severity
**High** (hang = whole class TIMEOUTs; historically blocked the entire JUnit-Platform Spring suite).

### Confidence
**Medium-high.** An in-tree comment names this exact test + loop; the guard is a non-registration invariant that CratonVM's own history shows can be violated by native shadowing. Cannot prove it currently fires without running, hence not "high".

### Recommendation
**Investigate/fix in CratonVM (likely a small handoff to the streams/dispatch owner).** Make the recursion structurally impossible rather than relying on a non-registration invariant: in `native_stream_to_array_gen` / `native_stream_to_array`, detect re-entry on the same `ReferencePipeline` ObjectRef (re-entrancy guard / depth cap) and fall back to real bytecode, OR confirm dispatch never resolves the interface-level `toArray` native for a concrete real pipeline. Add a regression test driving `MergedAnnotations.stream().toArray()` and `.toArray(Object[]::new)`.

---

## 2. org.springframework.core.codec.CharSequenceEncoderTests

### Symptom
Class TIMEOUTs. Passes on HotSpot/JDK25. Small class (3 test methods).

### What the test does
(`CharSequenceEncoderTests.java`, base `AbstractEncoderTests` in `spring-core/src/testFixtures/.../codec/AbstractEncoderTests.java`)
- `canEncode()` (line 51): pure synchronous `assertThat` — no reactive code. Unlikely to hang.
- `encode()` (line 69): `testEncodeAll(Flux.just(foo,bar), ...)` → runs FOUR reactive sub-scenarios, each via Reactor `StepVerifier`:
  - `testEncode` (`AbstractEncoderTests.java:152`) → `StepVerifier.create(result); stepConsumer.accept(step)` ending in `.verifyComplete()`.
  - `testEncodeError` (`:173`) → `Flux.concat(Flux.from(input).take(1), Flux.error(...))` then `StepVerifier.create(result)...expectError(...).verify()`.
  - `testEncodeCancel` (`:200`) → `StepVerifier.create(result).consumeNextWith(...).thenCancel().verify()`.
  - `testEncodeEmpty` (`:220`) → `Flux.empty()` then `StepVerifier.create(result).verifyComplete()`.
- `calculateCapacity()` (line 78): synchronous loop over charsets — not reactive.
- `@AfterEach checkForLeaks(Duration.ofSeconds(1))` (`AbstractLeakCheckingTests.java:46`) — a 1s bounded wait, not a hang source.

The reactive sources here are synchronous (`Flux.just`/`concat`/`error`/`empty`, no `.subscribeOn`/`.publishOn`), so on HotSpot `StepVerifier.verify()` completes inline. The hang therefore comes from a Reactor/JDK-concurrency primitive that CratonVM drives incorrectly so a terminal signal (onComplete/onError) or the verifier's own latch never resolves.

### Hang hypothesis (file:line)
`StepVerifier.verify()` blocks the calling thread on a `CountDownLatch` until the subscription terminates. By default Reactor's `StepVerifier.verify()` uses an **untimed** wait (no default global timeout), which maps to CratonVM's no-timeout latch:
- `native-builtins/src/lib.rs:25852` `native_cdl_await` — loops `monitor_wait(this, Some(10))` re-checking `cdl_count` until it hits 0. This is correct/non-spinning and times-bounded per iteration, so it does **not** hang by itself — **it hangs iff the count never reaches 0**, i.e. the producer side never runs `countDown()`.

Two concrete CratonVM defects can make the producer side never complete (either is sufficient):
1. **ExecutorService/scheduler runs tasks inline or not at all.** `ExecutorService.execute()` is wired to run the Runnable synchronously on the caller (`native-builtins/src/phases_late.rs:1087-1099`: `ctx.invoke_virtual(runnable,"run","()V",&[])`; mirror at `native-builtins/src/lib.rs:31328-31333` `native_es_execute`). If any Reactor path (or `StepVerifier`'s internal default scheduler) expects a *separate* thread to deliver a signal while the main thread is parked in `verify()`, the signal is never produced → latch never decremented → `native_cdl_await` loops forever.
2. **Scheduled/delayed tasks never fire while blocked.** The scheduled-task pump (`crate::scheduled_pump::registry().pump(ctx)`) is invoked from `Thread.sleep0` but **not** from the monitor wait loop. So a `StepVerifier` timeout/delay scheduled on `Schedulers.parallel()`/`single()` will not fire while the verifier is parked in `monitor_wait` → no timeout, no completion.

So: `verify()` parks in `native_cdl_await` (`lib.rs:25852`); the only thread that could `countDown()` either ran inline already-and-deadlocked, or is a scheduled task that never pumps → permanent hang.

Note: the timed `CountDownLatch.await(timeout)` (`native-builtins/src/lib.rs:25870-25901`) DOES honor its timeout correctly, so if `verify()` had used a finite timeout it would have thrown instead of hung. The untimed default is what turns a missing producer into a hang.

### How to confirm
1. Minimal repro (no Spring needed — isolate Reactor on CratonVM):
   ```java
   import reactor.core.publisher.Flux;
   import reactor.test.StepVerifier;
   public class StepVerifierRepro {
     public static void main(String[] a) {
       StepVerifier.create(Flux.just("foo","bar"))
         .expectNext("foo").expectNext("bar").verifyComplete();   // synchronous source
       System.out.println("ok-sync");
       StepVerifier.create(Flux.concat(Flux.just("x").take(1),
           Flux.error(new RuntimeException())))
         .expectNext("x").expectError().verify();                 // mirrors testEncodeError
       System.out.println("ok-error");
     }
   }
   ```
   If `ok-sync` prints but it hangs before `ok-error`, the cancel/error/concat path (separate-signal delivery) is the trigger.
2. Capture hang stack: `CRATONVM_DBG_HANGWALK=20`. Expect a frame parked in `native_cdl_await` / `monitor_wait` (confirms blocked-on-latch). Then determine the producer: search for a Reactor scheduler/executor task that should have run.
3. Cross-check: `ResourceRegionEncoderTests` (log line 334) also TIMEOUTs in the same codec package — a shared StepVerifier/Reactor root cause would explain both (worth verifying as the same bug).

### Suspected subsystem
Concurrency/threading + executor/scheduler semantics: `native-builtins/src/phases_late.rs:1087-1099` and `native-builtins/src/lib.rs:31328-31333` (inline `execute()`), the scheduled-task pump not being driven from the monitor wait loop, and `native_cdl_await` (`lib.rs:25852`) as the place the hang manifests. Plus whatever Reactor `Schedulers`/blocking-subscriber path is in play (Reactor jar not in this tree — needs runtime confirmation).

### Severity
**High** (class TIMEOUTs; likely the same root cause TIMEOUTs the sibling ResourceRegionEncoderTests, doubling the blast radius).

### Confidence
**Low-medium.** The mechanism (verify() → untimed latch → producer never runs) is well-grounded in CratonVM source, but exactly which Reactor producer path fails cannot be pinned without (a) the Reactor jar source and (b) a hang-stack. The CDL implementations themselves are correct, so the bug is upstream of the latch (executor/scheduler), which I could only narrow, not prove, statically.

### Recommendation
**Handoff to the threading/executor owner, after capturing a hang-stack** (`CRATONVM_DBG_HANGWALK`) on the minimal `StepVerifierRepro` to identify the missing producer. If confirmed: (a) make `ExecutorService.execute()` spawn a real thread via the existing `spawn_runnable_on_real_thread`/`ASYNC_POOL` path (`native-builtins/src/lib.rs:31368+`) instead of running inline, and/or (b) drive `scheduled_pump::registry().pump(ctx)` from the monitor-wait loop so delayed/scheduled tasks fire while a thread is parked. Do NOT "fix" by giving the latch a default timeout — that masks the deadlock. The shared symptom with ResourceRegionEncoderTests suggests one fix may clear both.
