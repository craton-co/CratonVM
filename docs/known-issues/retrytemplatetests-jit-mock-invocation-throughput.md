# `RetryTemplateTests` timing failures — Mockito mock-invocation overhead (NOT precise-JIT-maps)

Status: open — root cause corrected and substantially narrowed (2026-07-03 follow-up)

Date originally observed: 2026-07-03

## Summary

Three `org.springframework.core.retry.RetryTemplateTests$TimeoutTests` methods
fail under CratonVM real-JDK+JIT mode (HotSpot passes):

- `retryableWithTimeoutExceededAfterFirstDelayButBeforeFirstRetry`
- `retryableWithTimeoutExceededAfterFirstRetry`
- `retryableWithTimeoutExceededAfterSecondRetry`

All three configure a `RetryPolicy` with `timeout(Duration.ofMillis(20))` and
expect the retry infrastructure's own bookkeeping between the initial
`retryable.execute()` throw and the first `checkIfTimeoutExceeded` check
(`RetryTemplate.java:150`) — one Mockito `RetryListener` mock invocation, one
`RetryException` construct/throw/catch — to complete in near-zero wall-clock
time relative to the 20ms budget, as it does on HotSpot. Confirmed via direct
log inspection of a fresh run: e.g.
`retryableWithTimeoutExceededAfterFirstDelayButBeforeFirstRetry` gets the
*sleepTime=0* timeout message ("exceeded timeout (20ms); aborting execution")
instead of the expected *sleepTime=100* message ("would exceed timeout (20ms)
due to pending sleep time (100ms)..."), proving the **first**
`checkIfTimeoutExceeded(..., sleepTime=0, ...)` call already sees >20ms
elapsed — i.e. the one intervening mock call alone blew the budget.

## Root cause — CORRECTED (previous attribution to precise-JIT-maps was wrong)

**The previous version of this doc blamed default-on precise-JIT-maps codegen.
That attribution does not hold and was not actually re-verified against the
current build**: `precise_jit_maps_enabled()` (`jit/src/x64.rs`) has been
**default-OFF** since the BUG-01 fix (commit `d53c0e96`, well before this
investigation), and the `run-suite.sh` repro command never sets
`CRATONVM_PRECISE_JIT_MAPS=1`. Directly verified this run:

- `--verbose:gc` during the repro probe (below) emits **zero** GC lines —
  the wall-clock cost is not GC pauses.
- `--verbose:class` during the steady-state invocation loop emits **zero**
  class-load lines — not classloading.
- `--nojit` (interpreter-only) reproduces **the same magnitude** of slowdown
  as JIT-on (~100–170ms/call vs ~120–380ms/call) — ruling out JIT codegen
  (precise-maps or otherwise) as the mechanism, since the interpreter has no
  JIT codegen tax to pay at all.
- A synthetic call-chain-depth probe (40 levels of plain virtual-method
  calls) costs **<0.5ms** even at depth 40 under the same binary — ruling out
  a blanket "N-deep call chains are slow" interpreter tax as the explanation.
- Raw `ThreadLocal.get/set`, `synchronized`, `ReentrantLock`,
  `Method.invoke()`, `WeakHashMap.get`, `ConcurrentHashMap.put` are all
  sub-millisecond in isolation on this same binary — ruling out those
  specific JDK primitives.

**A `--stack-dump-on-timeout` capture taken mid-loop (thousands of samples
over a 45s window) shows the process is genuinely executing — repeatedly —
inside two specific, identifiable mechanisms, neither of which is precise
maps:**

1. **`org.mockito.internal.creation.bytebuddy.MockMethodAdvice.isOverridden()`**
   recomputing ByteBuddy's `MethodGraph.Compiler.Default` (`Collections
   .disjoint()` over generic `TypeToken` lists, `MethodGraph$Compiler
   $Default$Key.equals()/hashCode()`) via `net.bytebuddy.utility.dispatcher
   .JavaDispatcher` — a **JDK dynamic-proxy-backed reflection shim**
   (`jdk/proxy1/$Proxy4` → `JavaDispatcher$ProxiedInvocationHandler.invoke`
   → `java.lang.reflect.Method.invoke`) that ByteBuddy uses to stay portable
   across JDK versions. This appears to run in full on **every single**
   intercepted call rather than being cache-hit — this is third-party
   (vendored) ByteBuddy/Mockito bytecode, not CratonVM code.
2. **`org.mockito.internal.debugging.LocationFactory.create()` →
   `LocationImpl.getStackFrame()` → `java.lang.StackWalker.walk()`** —
   Mockito captures a `Location` (via the real JEP-259 `StackWalker` API,
   *not* legacy `Throwable.fillInStackTrace`) on **every** mock invocation,
   for use in verification-failure diagnostics. This routes into CratonVM's
   own native surface:
   `native-builtins/src/lang_stackwalker.rs::native_call_stack_walk` →
   `populate_sfi()`, which **allocates 8 heap objects per stack frame** (4
   strings + class mirror + `StackFrameInfo` + `StackTraceElement` — see the
   GC-safety comment at `lang_stackwalker.rs:96`), fed by
   `vm/src/vm/vm_exec.rs::capture_stack_trace` →
   `runtime::stackwalker::capture_full_trace`, which resolves
   `line_number_for_bci` (a `LineNumberTable` linear scan) for **every frame
   of the full live stack** — not just the frames the walk ultimately
   returns — while holding the class-manager read lock, then clones the
   whole vector into a per-thread `HashMap` before the caller's filter
   (`ordered_stack_walk_frames`) discards the walker-internal frames it
   doesn't need.

Isolated measurement of `StackWalker.walk()` alone (7-frame synthetic call
chain, no Mockito involved) on this binary: **4–437ms per call**, gradually
settling toward ~4.3ms after 5–8 repeated calls at the *same* call shape —
vs. HotSpot's legacy `Thread.getStackTrace()` and this same `StackWalker`
API both costing a fraction of a millisecond. A companion measurement of
legacy `new Throwable().getStackTrace()` at 1-frame depth was fast
(<0.1ms) on the same binary, confirming the cost is specific to the
JEP-259 `StackWalker` path Mockito's `LocationImpl` uses, not stack-trace
capture in general.

Combined, a fresh-mock-per-iteration call mirroring the real test's call
site (`listener.onRetryableExecution(eq(policy), eq(retryable),
argThat(...))`) costs **125–380ms** under CratonVM (`jit-real`, default
build) vs. **1–5ms** under HotSpot for the same probe — matching (and
exceeding) the originally-reported magnitude, but via a different, more
specific mechanism than previously documented.

## Why not fixed here

Both identified mechanisms are out of scope for a narrow, low-risk patch in
this pass:

1. **ByteBuddy's `isOverridden()`/`MethodGraph` cost** lives in vendored
   third-party bytecode (Mockito/ByteBuddy jars), not CratonVM source. The
   underlying reason this specific generic-type/`Collection`-heavy bytecode
   shape runs so much slower under CratonVM's interpreter than under HotSpot
   is **not yet root-caused to a specific bytecode pattern** — it is not GC,
   not classloading, not any of the individually-fast primitives tested
   above, and it reproduces identically under `--nojit`, so it is not a JIT
   codegen tax either. Pinning this down further is genuinely a *new*
   interpreter-throughput investigation (distinct from BUG-01's precise-maps
   codegen tax), not a same-session fix.
2. **`StackWalker`/`capture_stack_trace` per-frame allocation cost** *is*
   CratonVM's own code (`lang_stackwalker.rs`, `vm_exec.rs`), and is in
   principle a legitimate optimization target (e.g. lazily building the
   `StackTraceElement` mirror instead of eagerly allocating it per frame in
   `populate_sfi`, or resolving line numbers only for frames actually
   returned rather than the full live stack). However, this exact code path
   is **heavily hardened against specific, previously-shipped regressions**
   documented inline in its own comments — a moving-GC use-after-free SIGSEGV
   (`ByteArrayMappingTests`), a Spring Boot banner NPE
   (`deduceMainApplicationClass`), and a log4j2 caller-class cache-miss
   recursion (`getCallerClass`) — all of which were fixed by *adding* the
   eager per-frame work this investigation would need to trim. Changing it
   without full-suite regression verification across those specific prior
   incidents risks reintroducing them; that verification is beyond a single
   bug-fix pass and is the same caution this doc's previous (precise-maps)
   version correctly applied, just to the wrong specific mechanism.

No JIT/precise-maps flags were touched. No code changes were made in this
pass — this update is profiling data only, verified against a fresh build of
current `dev` tip (reproduces identically: `found=24 succ=21 fail=3`,
matching the frozen baseline).

## Repro

```bash
cd apps/spring-suite-runner
export PATH=/usr/bin:/bin:$PATH
CRATONVM_BIN=$PWD/vmfrozen/cratonvm-rerun.exe KRUN_STACK=1 \
  ./run-suite.sh run --jdk real --jit on --batch 1 --only 'core\.retry\.RetryTemplateTests'
```

Diagnostic commands used in this investigation (GC/class/stack-dump
correlation): `--verbose:gc`, `--verbose:class`, `--nojit`, and
`--stack-dump-on-timeout <N>` (dumps the interpreter frame chain of every
thread after `N` seconds and aborts — invaluable for catching a slow call
*in the act* rather than guessing from wall-clock deltas alone).

## Suggested next steps

Two independent follow-ups, neither belonging to the precise-maps/BUG-01
workstream:

1. **New interpreter-throughput lead**: root-cause why ByteBuddy's
   `MethodGraph.Compiler`-style generic/`Collection`-heavy code
   (`Collections.disjoint()`, `TypeToken.equals/hashCode`, `ParameterList`
   wrapping via JDK dynamic proxies) is disproportionately slow under
   CratonVM's interpreter specifically — reproduces under `--nojit`, so it's
   an interpreter dispatch cost, not JIT codegen. Worth hand-off to whoever
   owns interpreter throughput, with this doc's stack-dump evidence as a
   starting point.
2. **`StackWalker` per-frame allocation cost**: consider lazy
   `StackTraceElement` construction and skipping full-stack line-number
   resolution for frames beyond what a walk ultimately consumes in
   `lang_stackwalker.rs` / `vm_exec.rs::capture_stack_trace` — but only with
   full regression coverage of the three specific historical incidents
   documented inline in `lang_stackwalker.rs` (ByteArrayMappingTests SIGSEGV,
   Spring Boot banner NPE, log4j2 caller-class recursion) before landing.

Once either lands, re-run this class before concluding it needs a test-side
fix — these three tests are also inherently timing-fragile (a 20ms budget
around framework-mock-invocation + exception handling) even on a
correct-but-slower JVM implementation.
