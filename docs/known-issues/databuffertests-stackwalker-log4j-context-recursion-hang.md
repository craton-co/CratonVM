# `DataBufferTests` hang — Log4j2/Netty bootstrap drowns in `StackWalker` overhead (shared root cause with RetryTemplateTests)

Status: open

Date observed: 2026-07-03 (updated 2026-07-04 after a second investigation pass)

## A concurrent fix attempt did NOT resolve this (verified 2026-07-04)

A separate, concurrent session investigated a similarly-shaped StackWalker/
Log4j2 issue and concluded it was fixed (see
`docs/internal/databuffertests-stackwalker-log4j-context-recursion-hang.md`,
added alongside `vm/tests/wp1_9_stackwalker.rs`). That session's own
write-up states it did **not** verify against the real Spring test suite —
`apps/spring-suite-runner`/`apps/spring-framework` were not present in their
checkout — and instead validated a hand-rolled synthetic probe fixture plus
a standalone Netty micro-test that merely resembles the failure shape.

Built the actual merged `dev` (including that session's changes) and reran
the real test:

```bash
cd apps/spring-suite-runner
CRATONVM_BIN=$PWD/vmfrozen/cratonvm-swverify.exe KRUN_STACK=1 \
  ./run-suite.sh run --jdk real --jit on --batch 1 --batch-to 300 --one-to 300 \
  --only 'core\.io\.buffer\.DataBufferTests'
```

Result: **still `TIMEOUT`** (`wall=602s`, `found=0 passed=0 failed=0` — zero
test methods completed). The synthetic-probe-based fix does not cover
whatever makes the real Spring test class's bootstrap sequence trigger this
at a scale/shape the probe didn't reproduce. This doc stays in
`docs/known-issues/` (not `docs/internal/`) because the real, originally-
reported failure is confirmed still present on current `dev`.

## Summary

`org.springframework.core.io.buffer.DataBufferTests` times out under CratonVM
real-JDK+JIT mode (HotSpot passes) — confirmed to exceed **60+ minutes**
across two sequential 30-minute attempts (batch + crash-recovery re-run),
never completing, never producing partial test results. The stall happens
during `@BeforeEach`/class-setup
(`AbstractDataBufferAllocatingTests.createAllocators()` → Netty's
`AbstractByteBufAllocator.<clinit>` → `ResourceLeakDetector.<clinit>` →
Log4j2 `InternalLoggerFactory` → `LogManager.<clinit>` →
`ProviderUtil.lazyInit()` → `StackLocatorUtil.getCallerClass` →
`java.lang.StackWalker.walk()`), **before any DataBuffer test logic runs**.

An earlier investigation attempt (no live repro, guesswork only)
hypothesized the hang was inside `DataBufferInputStream.read()`/
`available()`; that is **refuted** — live thread dumps show the process
entirely inside `StackWalker`/Log4j2 caller resolution, unrelated to
`DataBuffer` I/O.

**This is the same underlying performance root cause documented in
[[retrytemplatetests-precise-maps-attribution-refuted]]** (found
independently, in a parallel investigation of a different test class):
`java.lang.StackWalker.walk()` in CratonVM real-JDK mode is expensive per
call — `native-builtins/src/lang_stackwalker.rs::populate_sfi` does ~8 heap
allocations per stack frame, fed by
`vm/src/vm/vm_exec.rs::capture_stack_trace`, which resolves line numbers for
the **entire live stack** (not just the frames actually consumed) while
holding the class-manager read lock. Log4j2's own internal bootstrap
(`ProviderUtil.lazyInit()`, `PropertiesUtil`'s lazy static init, and
whatever else transitively calls `StackLocatorUtil.getCallerClass`) performs
**many** `StackWalker.walk()` calls, each against a ~75-98 frame stack, as
part of its own one-time logging-provider/config discovery.

## Repro

```bash
cd apps/spring-suite-runner
export PATH=/usr/bin:/bin:$PATH
KRUN_W="$(cygpath -m "$PWD")"
cpf="../spring-framework/spring-core/build/cratonvm-testcp.txt"
mcp="$KRUN_W;$(tr -d '\r' < "$cpf")"
./vmfrozen/cratonvm-rerun.exe --java-home "C:\Program Files\Microsoft\jdk-25.0.3.9-hotspot" \
  --stack-dump-on-timeout 15 -cp "$mcp" KRun org.springframework.core.io.buffer.DataBufferTests
```

`CRATONVM_DEBUG_STACKWALK=1` adds per-frame class/method tracing to every
`callStackWalk`/`fetchStackFrames` call (see `lang_stackwalker.rs`).

## Evidence

**Extended timeout test (this pass):** ran with `--batch-to 1800 --one-to
1800` (30 min per attempt). Result: `TIMEOUT`, `wall=3686s` (the batch
attempt AND the crash-recovery lone re-run both burned their full 1800s
budget) — **zero test methods completed** in over an hour. This rules out
"just slow but would finish in a few minutes" — whatever is happening here
is either genuinely unbounded, or bounded by a count so large it's
practically unbounded for test-suite purposes.

**Per-cycle trace detail (`CRATONVM_DEBUG_STACKWALK=1`, ~20 distinct
`callStackWalk` cycles captured in ~90s before intentionally killing the
process):** Each cycle is `StackWalker.walk()` populating a fresh ~75-98
frame trace. Critically, **cycle contents are NOT identical repeats of the
same call** — e.g. cycle 1's frame list shows `LogManager.<clinit> →
ProviderUtil.getProvider → ProviderUtil.lazyInit → ServiceLoaderUtil.safeStream
→ StackLocatorUtil.getCallerClass → StackLocator.getCallerClass` (a short
chain), while cycle 2 goes deeper: the SAME `ProviderUtil.lazyInit()`
frame now sits above an *additional* internal chain —
`PropertiesUtil.getProperties → Lazy.get → LazyUtil$SafeLazy.value →
PropertiesUtil.lambda$static$0 → PropertiesUtil.<init> (x2) →
PropertiesUtil$Environment.<init> (x2) → ServiceLoaderUtil.safeStream →
StackLocatorUtil.getCallerClass → StackLocator.getCallerClass`. This proves
`ProviderUtil.lazyInit()`'s **own single execution** legitimately triggers
multiple *different* `StackWalker.walk()` calls at different points in its
body (once directly for provider discovery, again transitively via
`PropertiesUtil`'s own lazy static init doing its own provider/property
lookup) — this is **not** a tight single-frame spin, and is (at least in
part) genuinely necessary, if slow, work that HotSpot also performs (just in
microseconds).

**Reconciling "not a tight spin" with "60+ minutes, never completes":**
`javap`-disassembled `ProviderUtil.lazyInit()` (log4j-api-2.26.0.jar) DOES
have a run-once guard (`getstatic PROVIDER; ifnonnull <return>` at offset 0)
— if `PROVIDER` were correctly set after the first successful run,
`lazyInit()` should short-circuit immediately on every subsequent call. Its
reappearance in essentially every observed cycle (not just the first few)
suggests either: (a) `PROVIDER`'s `putstatic` write is never actually
reached (something throws before line ~115 of `lazyInit()`, but per JVMS
§5.5 a `<clinit>`-chain failure should be cached as
`ClassState::InitializationError` — confirmed CratonVM implements this
correctly and by default does NOT retry, per `vm/src/vm/vm_util.rs`'s
`<clinit>` failure-policy doc — so a silent-retry-forever explanation would
itself be a second, deeper bug in class-init state tracking, not yet
confirmed); or (b) something about the sheer volume of legitimately-distinct
one-time work here (many discrete logger/provider/property lookups, each
needing its own slow `StackWalker` walk) is simply far larger in count than
initially assumed, and the true bottleneck is `capture_stack_trace`'s
full-stack line-resolution cost times a very large (not literally infinite,
but practically so within test-suite timeouts) number of legitimate calls.
Not conclusively distinguished between (a) and (b) in this pass.

## Why not fixed here

Two independent investigations (this one, and the `RetryTemplateTests` one)
converge on the same underlying expensive code path:
`native-builtins/src/lang_stackwalker.rs` (`populate_sfi`,
`capture_stack_trace`). That code is **explicitly hardened against three
documented past regressions** (a moving-GC SIGSEGV in Hibernate
`ByteArrayMappingTests`, a Spring Boot banner NPE, and — notably — a
previous log4j2 caller-class-recursion incident, already partially
mitigated via the `SW_FRAME_CACHE` clean-frame-list mechanism). Changing its
per-frame allocation eagerness or `capture_stack_trace`'s full-stack
resolution needs full-suite regression coverage against all three incidents
before landing — not a same-session patch, and explicitly flagged as such
by the `RetryTemplateTests` investigation already. Attempting a rushed fix
here risks reintroducing any of those three regressions.

If (a) above (a genuine class-init-state or Log4j2-side caching bug causing
true unbounded re-execution) turns out to be the real story rather than (b)
(merely a very large bounded count), that would be a narrower, more
tractable, and lower-risk fix than touching `lang_stackwalker.rs`'s
performance-sensitive internals — but distinguishing the two needs more
investigation than this pass had budget for.

## Suggested next steps

1. **Distinguish (a) vs (b) first** before touching any shared code:
   instrument `ProviderUtil.PROVIDER`'s static field (or more generally,
   whatever `LogManager`/`ProviderUtil`/`PropertiesUtil` class-init state
   looks like) across successive `callStackWalk` cycles — if the SAME class
   keeps re-running full initialization instead of short-circuiting on a
   cached value, that is a distinct, more narrowly-fixable bug (likely in
   class-init state tracking or field persistence, not StackWalker
   performance) separate from the shared throughput issue.
2. If it's confirmed as case (b) (genuinely bounded-but-enormous legitimate
   work, no logic bug), this doc should be merged/cross-referenced with the
   `RetryTemplateTests` throughput work — a single shared
   `lang_stackwalker.rs` throughput improvement (after the required 3-incident
   regression soak) would very likely fix both.
3. A minimal standalone probe (Netty `ResourceLeakDetector` class-init only,
   with a real log4j2 classpath, no Spring/JUnit) would remove test-harness
   noise and make it much cheaper to iterate on either hypothesis above.
