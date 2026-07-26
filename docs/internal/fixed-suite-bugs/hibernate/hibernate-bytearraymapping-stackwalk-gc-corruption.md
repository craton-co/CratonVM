# `ByteArrayMappingTests` SIGSEGV — log4j2 `getCallerClass` → `StackWalker.walk` GC corruption

**Severity:** High (hard, uncatchable crash, `EXCEPTION_ACCESS_VIOLATION` / SIGSEGV, rc=139).
**Status:** FIXED on `codex/hib-bytearray-stackwalk-gc-20260703`. The earlier "re-entrant non-termination" diagnosis (below) was **WRONG**: this was GC memory corruption during the real-JDK `StackWalker` walk. The final residual was the lazy `StackFrameBuffer.fill` reflective-constructor path holding the `StackWalker` constructor argument only in native Rust locals across allocation/class-init GC.
**Fix:** `NativeContextImpl::{new_object_initialized,new_object_initialized_with_class_id}` now pins object constructor arguments and re-reads them from `native_pin_roots` before dispatching `<init>`, so reflective constructors receive post-GC object references.
**Validation (2026-07-04):** `ByteArrayMappingTests` solo, `--nojit --Xmx 1500m`, unique binary `cratonvm-hib-bytearray-stackwalk-gc-20260703.exe`: `@@RESULT 0 org.hibernate.orm.test.mapping.basic.ByteArrayMappingTests found=2 started=2 ok=2 failed=0 aborted=0 skipped=0`.
**Note:** The validation still emits guarded `HIB-CV-32` corrupt-Value diagnostics during BLOB bind/extract logging; those degrade to null and are tracked separately in the archived HIB-CV-32 write-up, not this StackWalker crash.
**Mode:** Interpreter (JIT-off, and JIT-on). **HotSpot (JDK 25):** PASS.

## Corrected diagnosis (2026-06-20 — supersedes the "re-entrant loop" section below)

It is **NOT** a re-entrant / non-terminating `StackWalker.walk`. Evidence: walks are NOT nested
(`CRATONVM_DBG_SWREENTER`=0); the "captured stack grows each round 42→111" is just Hibernate startup
nesting progressively deeper, not re-entrancy; `getCallerClass` returns the CORRECT class (toy `CallerWalk`
matches HotSpot); only ~63 walks, depth maxes ~111. The crash is **GC corruption** — proven with
`CRATONVM_DBG_GC_STRESS=1` (SIGSEGV → rc=1 with `gen_heap` `ClassId(0) num_slots=0` zeroed-header warnings =
collected objects accessed). Heap-size-invariant (8g still crashes) and stack-size-invariant
(`-Xss`-equivalent 1g still crashes → not native-stack overflow; H7 correctly never fires).

**Root cause class:** CV native code holds Java object refs in Rust locals/`Vec`s across allocating `ctx`
calls (`invoke_virtual` / `create_string` / `alloc_*`) **without re-reading them from a pin** afterwards.
`safe_native_call` pins a native's *args*, but the native's local copies still go stale when the moving
young-gen GC relocates the object (the pin is remapped; the bare local is not). The `StackWalker` walk
drives this relentlessly (one `StackFrameInfo`+`StackTraceElement`+strings per frame via `populate_sfi`).

**Fixed in this branch** (`fix/hib-stackwalk-reentrant-loop`):
1. `native_fetch_stack_frames` was **re-capturing** the live stack (deeper, polluted with `java.util.stream.*`
   / `StackStreamFactory$*` frames) and indexing it with the `callStackWalk`-relative cursor → fed the user
   function garbage frames. Fix: `SW_FRAME_CACHE` caches the clean ordered list at `callStackWalk`.
2. `populate_sfi` / `populate_stack_frame` / `p59_sw_walk` / the `callStackWalk`+`fetchStackFrames` natives:
   pinned all in-flight allocations (`sf` / strings / mirror / `ste` / the frame buffer array).
3. `native_stream_spliterator` / `stream_elements` / `materialize_lazy_stream` (native-collections): the
   eager synthetic-stream path drained the StackWalker spliterator (heavy alloc → moving GC) then wrote the
   result back through a **stale** `stream`/`elements`/`arr`. Pinned + re-read via `read_native_pin`
   (`pin_value_slice` / `read_value_slice` helpers). This fixes `DeepWalkGC.java` (rc=1+corruption → rc=0).

Pin-pointing tool added: `CRATONVM_DBG_STRAYSTACK=1` now prints `CULPRIT-NATIVE=… RVA=0x…` for any native
that writes through a relocated/zeroed receiver — symbolize with `CRATONVM_SYMBOLIZE`.

Minimal repro for the (now-fixed) stream-native layer — `cratonvm --nojit --Xmx 64m -cp . DeepWalkGC`
(was rc=1 + `ClassId(0)` zeroed-header warnings; now `caller=DeepWalkGC$JLogger`, rc=0). `DeepWalkGC2`
is the same with log4j2's exact non-allocating predicates (`getClassName().equals/startsWith`):
```java
import java.lang.StackWalker.StackFrame; import java.lang.StackWalker.Option;
public class DeepWalkGC {
    static final StackWalker WALKER = StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE);
    static Class<?> getCallerClass(String fqcn) {                 // log4j2 java9 StackLocator pattern
        return WALKER.walk(s -> { java.util.ArrayList<byte[]> junk = new java.util.ArrayList<>();
            return s.dropWhile(f -> { junk.add(new byte[64]); return !f.getClassName().equals(fqcn); })
                    .dropWhile(f -> f.getClassName().equals(fqcn))
                    .dropWhile(f -> !f.getClassName().startsWith("")).findFirst(); })
            .map(StackFrame::getDeclaringClass).orElse(null);
    }
    static class LogMgr { static Class<?> getContext() { return getCallerClass(LogMgr.class.getName()); } }
    static class JLogger { final Class<?> c; JLogger() { c = LogMgr.getContext(); } }
    static int sink;
    static Class<?> deep(int n) { if (n>0){ byte[] b=new byte[32]; sink+=b.length; return deep(n-1);}
        Class<?> last=null; for (int i=0;i<2000;i++) last=new JLogger().c; return last; }
    public static void main(String[] x){ System.out.println("caller="+deep(95).getName()); }
}
```

## Fixed residual (2026-07-04)

The remaining hard crash was in the real-JDK lazy drain path (`Stream.dropWhile` -> `tryAdvance` -> `StackFrameBuffer.fetchStackFrames()` -> `resize` -> `fill`). `StackFrameBuffer.fill` reflectively constructs `StackFrameInfo(StackWalker)`. `Constructor.newInstance(walker)` read `walker` from the native argument array, then passed it to `new_object_initialized_with_class_id` as an unrooted Rust-local `Value`.

A moving young GC during object allocation or class initialization could relocate that `StackWalker` before the Java `<init>` frame copied the argument into scanned locals. The constructor then read a stale object reference, corrupting the value-stack/frame state and eventually crashing in the fill loop. The construction helpers now root object constructor arguments across allocation/class initialization and rebuild the `<init>` argument vector from remapped pin handles immediately before dispatch.

Validated with the full Hibernate fixture: `ByteArrayMappingTests` now reports `found=2 started=2 ok=2 failed=0 aborted=0 skipped=0` and exits normally.

---
## (Superseded) original hypothesis — re-entrant non-termination

## What it actually is (confirmed via instrumentation — supersedes earlier hypotheses)

It is **not** the `Byte[]`→H2-array binding, and **not** a stack overflow. It is **log4j2's
`StackLocator.getCallerClass`** using **`StackWalker.walk(s -> s.dropWhile(...)...)`** during Hibernate's
logging bootstrap (`BootLogging.<clinit>`), where the walk **re-enters itself without terminating**.

Full Java caller chain captured at the spin (one-shot `CRATONVM_DBG_SOE` dump):
```
#0  java/util/stream/Stream.dropWhile
#1  org/apache/logging/log4j/util/StackLocator.lambda$getCallerClass$6
#2  java/lang/StackStreamFactory$StackFrameTraverser.consumeFrames
#3  java/lang/StackStreamFactory$AbstractStackWalker.beginStackWalk
#4  java/lang/StackStreamFactory$AbstractStackWalker.walkHelper
#5  org/apache/logging/log4j/util/StackLocator.getCallerClass
…   org/jboss/logging/… → org/hibernate/boot/BootLogging.<clinit>
```

`CRATONVM_DEBUG_STACKWALK=1` shows the smoking gun — **the same walk runs hundreds of times (777 SW-DBG
events), and the captured stack GROWS each round** until a wild dereference faults:
```
callStackWalk consumed=2 … ordered_len=83 trace_len=91
fetchStackFrames anchor=2 → 7 → 18 → 41 → 72       (one walk consumes all 83 frames — correct)
callStackWalk capture len=101 … ordered_len=99     ← a NEW walk; stack grew 83 → 99
fetchStackFrames anchor=2 → 7 → 18 → 41 → 72 …      ordered_len=99 → grows again → …
```
Each complete walk leaves ~16 extra frames that the next walk sees (83 → 99 → …). The `fetchStackFrames`
cursor advances correctly *within* a walk (the JDK FrameBuffer doubles batch size 5,11,23,31…), so the
per-walk batch protocol is fine — the bug is that **a fresh `StackWalker.walk` is launched from inside the
walk's own frame processing**, unboundedly. EXEC_DEPTH stays ~24 and native `remaining` stays at the full
128 MiB throughout, confirming it is NOT native-stack exhaustion.

## Why it doesn't reproduce in a toy repro

`jsonrepro/{SWDrop,RecurseSW,SWDropAll}.java` exercise `StackWalker.walk(s -> s.dropWhile(...).findFirst())`
(including never-matching predicates and 60-deep stacks) and all **complete cleanly** on CratonVM, matching
HotSpot. The re-entrancy only arises in the real log4j2 bootstrap, where resolving a walked frame
(`populate_sfi` → `getDeclaringClass` / class init / `org.jboss.logging` → log4j2) re-enters
`getCallerClass` → `StackWalker.walk`. The CV defect is most likely that CV's synthetic `StackFrameInfo`
(`../../../../native-builtins/src/lang_stackwalker.rs`, 6-slot layout) returns a caller class log4j2 doesn't accept, so
its `ClassLoaderContextSelector.getContext` re-initialises and walks again — an infinite re-init loop that
HotSpot avoids because its `getDeclaringClass`/`getCallerClass` return the expected frame.

## Root fix (handoff)

Trace one full walk→walk boundary (what re-invokes `getCallerClass`): instrument `native_fetch_stack_frames`
/ `populate_sfi` to log the class each StackFrameInfo resolves to, and compare against HotSpot for the same
bootstrap. Likely fixes: ensure `StackWalker$StackFrame.getDeclaringClass()` / `getClassName()` on CV's
synthetic frames return exactly what log4j2's `dropWhile` predicate expects (so the walk finds the caller and
terminates), or break the logging re-entrancy during frame resolution. Not a localized one-liner.

## H7 — native stack-bang (general mitigation, landed separately)

While diagnosing, a real **SP-based stack-bang** was added to `execute` (`interpreter.rs`): it probes the
actual remaining native stack via `GetCurrentThreadStackLimits` and throws a *catchable*
`StackOverflowError` within a 1 MiB margin, converting the class of **uncatchable native-stack-overflow
SIGSEGVs** (e.g. deep ByteBuddy reflection cascades the H6 per-level counter can't size correctly) into
recoverable Java errors. Default-on; opt out `CRATONVM_NO_STACK_BANG=1`; tune `CRATONVM_STACK_BANG_MARGIN_KB`.
Verified: catches genuine overflow when the H6 counter is disabled; does not false-trip legit recursion
(simple infinite recursion still caught by the existing Java-frame guard; binaryTrees unaffected). **This
mitigation does NOT fix `ByteArrayMappingTests`** — that crash is a re-entrant loop, not stack exhaustion.

### Diagnostic recipe
- `--profile release-with-debug`; `CRATONVM_SYMBOLIZE=0x<rva>,… cratonvm` to symbolize VEH frames.
- `CRATONVM_DBG_SOE=1` + the H7 `dropWhile`/depth hook → one-shot Java caller chain.
- `CRATONVM_DEBUG_STACKWALK=1` → the repeated `callStackWalk` / growing `ordered_len` loop signature.
- Repro: single class `mapping.basic.ByteArrayMappingTests` (JIT-off), reproduces strictly solo.
