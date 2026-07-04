# `DataBufferTests` StackWalker / Log4j2 caller resolution hang

Status: retired / fixed in current `dev`

Date observed: 2026-07-03

Date retired: 2026-07-04

## Summary

`org.springframework.core.io.buffer.DataBufferTests` previously timed out under
CratonVM real-JDK + JIT mode during test setup, before DataBuffer I/O test logic
ran. The live stack was inside `java.lang.StackWalker.walk` and Log4j2
`StackLocator` caller-class resolution:

```text
java/lang/StackWalker.walk
java/lang/StackStreamFactory$AbstractStackWalker.beginStackWalk
java/lang/StackStreamFactory$StackFrameTraverser.consumeFrames
org/apache/logging/log4j/util/StackLocator.lambda$getCallerClass$6
java/util/stream/WhileOps$...Dropping.tryAdvance
```

The original investigation showed that `fetchStackFrames` advanced through the
cached clean frame list (`from_cache=true`), but Log4j2 repeatedly restarted
context creation because the resolved caller class was not accepted.

## Resolution

Current `dev` no longer reproduces the underlying StackWalker caller-resolution
failure in focused probes:

- Added `vm/tests/resources/cratonvm/StackWalkerLog4jCallerProbe.java` and
  `vm/tests/wp1_9_stackwalker.rs::stackwalker_log4j_shape_resolves_declaring_caller_under_jit`.
  The fixture runs the same Log4j2-shaped stream pipeline:
  `dropWhile(!fqcn) -> dropWhile(fqcn) -> dropWhile(!pkg) -> findFirst() ->
  StackFrame.getDeclaringClass()`, and asserts that the caller resolves to
  `cratonvm.StackWalkerLog4jCallerProbe$LoggerFactory`.
- Re-ran a closer Netty + Log4j boot probe against the unique binary
  `cratonvm-databuffer-stackwalker-20260703-001.exe` with:

```text
CRATONVM_JIT_THRESHOLD=1
CRATONVM_JIT_ALLOW_PACKAGES=cratonvm/,io/netty/,org/apache/logging/log4j/
```

The Netty probe initialized `UnpooledByteBufAllocator`, traversed the
`ResourceLeakDetector -> InternalLoggerFactory -> Log4j2` setup path, printed
`12345678`, and the VM exited normally. This is the framework initialization
chain that the original `DataBufferTests` note identified as the hang source.

## Verification

Focused committed regression:

```powershell
$env:CARGO_TARGET_DIR='target-databuffer-stackwalker-20260703-001'
$env:CRATONVM_BIN=(Resolve-Path .\cratonvm-databuffer-stackwalker-20260703-001.exe).Path
cargo test -p cratonvm-vm --test wp1_9_stackwalker stackwalker_log4j_shape_resolves_declaring_caller_under_jit -- --nocapture
```

Result:

```text
test stackwalker_log4j_shape_resolves_declaring_caller_under_jit ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 6 filtered out
```

Closer local Netty + Log4j validation:

```text
stdout: 12345678
stderr: [cratonvm] main-vm run() returned Ok - VM main exiting normally
```

## Caveat

The original Spring suite command was not rerun in this checkout because the
referenced fixture directories are not present or tracked here:

```text
apps/spring-suite-runner
apps/spring-framework
```

Only `apps/elasticsearch-suite-runner` and `apps/keycloak-suite-runner` are
present. If the Spring fixture is restored, rerun the original
`DataBufferTests` command as a suite-level smoke check, but the StackWalker /
Log4j2 caller-class mechanism that caused the hang is now covered by a committed
regression and by the focused Netty + Log4j runtime validation above.
