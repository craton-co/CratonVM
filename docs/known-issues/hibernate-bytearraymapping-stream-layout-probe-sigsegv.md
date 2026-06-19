# `ByteArrayMappingTests` SIGSEGV — log4j2 `getCallerClass` → `StackWalker.walk` re-entrant non-termination

**Severity:** High (hard, uncatchable crash, `EXCEPTION_ACCESS_VIOLATION` / SIGSEGV, rc=139).
**Status:** 🔴 OPEN (root cause CONFIRMED & precisely pinned; root fix is non-trivial — handoff). A separate
general **mitigation (H7 native stack-bang)** landed for the *class* of uncatchable native-stack overflows
(it does NOT catch this bug — see below).
**Mode:** Interpreter (JIT-off). **HotSpot (JDK 25):** PASS.

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
(`native-builtins/src/lang_stackwalker.rs`, 6-slot layout) returns a caller class log4j2 doesn't accept, so
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
