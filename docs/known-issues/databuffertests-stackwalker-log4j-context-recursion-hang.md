# `DataBufferTests` hang — `StackWalker`-driven Log4j2 caller-class resolution never terminates

Status: open

Date observed: 2026-07-03

## Summary

`org.springframework.core.io.buffer.DataBufferTests` times out under CratonVM
real-JDK+JIT mode (HotSpot passes). The hang happens during
`@BeforeEach`/class-setup (`AbstractDataBufferAllocatingTests.createAllocators()`
→ Netty's `AbstractByteBufAllocator.<clinit>` → `ResourceLeakDetector.<clinit>`
→ Log4j2 `InternalLoggerFactory`/`Log4jContextFactory.getContext` →
`ClassLoaderContextSelector.getContext` → Log4j2's
`StackLocatorUtil.getCallerClass`), **before any DataBuffer test logic runs**.
An earlier investigation attempt (no live repro, guesswork only) hypothesized
the hang was inside `DataBufferInputStream.read()`/`available()`; that is
**refuted** — a live thread dump (see below) shows the hang is entirely in
`java.lang.StackWalker` / Log4j2 caller resolution, unrelated to `DataBuffer`
I/O.

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

## Evidence

Two stack dumps taken 15s apart pin the "main" thread at the identical frame:

```
tid=0 depth=79 class=java/lang/StackWalker method=walk desc=(Ljava/util/function/Function;)Ljava/lang/Object;
tid=0 depth=80 class=java/lang/StackStreamFactory$AbstractStackWalker method=walkHelper
tid=0 depth=81 class=java/lang/StackStreamFactory$AbstractStackWalker method=beginStackWalk
tid=0 depth=82 class=java/lang/StackStreamFactory$AbstractStackWalker method=doStackWalk
tid=0 depth=83 class=java/lang/StackStreamFactory$StackFrameTraverser method=consumeFrames
tid=0 depth=84 class=org/apache/logging/log4j/util/StackLocator method=lambda$getCallerClass$6
tid=0 depth=85 class=java/util/stream/WhileOps$UnorderedWhileSpliterator$OfRef$Dropping method=tryAdvance pc=0 last_pc=0
```

`CRATONVM_DEBUG_STACKWALK=1` tracing (see `native-builtins/src/lang_stackwalker.rs`)
shows the walk is *not* stuck at a fixed cursor — `anchor` genuinely advances
within each `callStackWalk` (e.g. 2→7→18→41→72 against `ordered_len=75`) — but
`callStackWalk` itself is then invoked *again from scratch* repeatedly
(`capture len=79, 87, 82, 98, 88, 88` across ~6 cycles in 40s), i.e. Log4j2's
`getCallerClass` is being re-entered from a new logging/context-creation call
each time. This matches a failure mode already documented in
`lang_stackwalker.rs`'s `SW_FRAME_CACHE` comment: *"a wrong/internal caller
means the cache never hits, so each log call re-creates a context and
re-logs → unbounded context-creation recursion → native-stack/value-stack
corruption (Hibernate `ByteArrayMappingTests` SIGSEGV)"* — that comment
describes the exact symptom (previously fixed for one manifestation via the
`SW_FRAME_CACHE` clean-frame-list mechanism), but it still reproduces here for
`DataBufferTests`'s specific Netty/Log4j2 boot sequence: the clean cached
frames are fed to the walk correctly (`from_cache=true`), yet the outer
Log4j2 caller-resolution logic apparently still never gets a caller class it
considers valid, so it keeps re-creating `LoggerContext`s indefinitely.

Only 6 `callStackWalk` cycles occurred in 40s (each cycle takes multiple
seconds — likely `populate_sfi`'s per-frame allocation cost against a ~80-90
frame stack, run repeatedly), so this reproduces as a genuine long/likely
unbounded hang, not merely slow-but-finite progress within the suite
runner's default 600s batch timeout.

## Why not fixed here

Root-causing exactly why Log4j2's caller-class predicate never accepts an
answer (a `Class` identity/equality mismatch against CratonVM's frame
`Class` mirrors, vs. a genuine Log4j2-side retry-forever policy) requires
deeper investigation than this session's scope for the `core.*` cluster
allowed, and any fix in this area touches the shared `StackWalker`/GC-root
frame-cache machinery (already the subject of a prior SIGSEGV fix — see the
comment above) — not a good candidate for a rushed change. Flagging here per
`docs/known-issues` convention for follow-up.

## Suggested next steps

- Confirm whether `StackFrameInfo.getDeclaringClass()` (as produced by
  `populate_sfi` in `native-builtins/src/lang_stackwalker.rs`) round-trips
  through `Class` identity comparisons the way Log4j2's
  `StackLocator.getCallerClass(String fqcn, ...)` expects (it needs to find
  the exact `fqcn` class in the walked frames to anchor its `dropWhile`
  chain).
- Consider a bounded retry/give-up policy check on the Log4j2/Netty side is
  not something CratonVM can change (real bytecode) — the fix has to make
  the *first* `getCallerClass` call succeed.
- If reproducible in isolation, a minimal standalone probe (Netty
  `ResourceLeakDetector` class-init only, no Spring) would remove JUnit/Spring
  noise from the stack and might make it easier to inspect what caller class
  is actually being resolved at each cycle (e.g. via `CRATONVM_ANN_TRACE`-style
  ad hoc tracing added to `StackLocator`'s dropWhile predicate call site).
