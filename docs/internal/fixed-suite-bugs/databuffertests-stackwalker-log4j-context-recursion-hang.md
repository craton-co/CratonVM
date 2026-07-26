# `DataBufferTests` StackWalker / Log4j2 caller resolution hang

Status: fixed in current `dev`

Date observed: 2026-07-03

Date retired: 2026-07-05

## Summary

`org.springframework.core.io.buffer.DataBufferTests` timed out under CratonVM
real-JDK + JIT mode during setup, before DataBuffer I/O test logic ran. Thread
dumps showed Netty bootstrap entering Log4j2 provider discovery and repeatedly
resolving caller classes through:

```text
ProviderUtil.lazyInit()
ServiceLoaderUtil.safeStream()
StackLocatorUtil.getCallerClass(...)
StackLocator.getCallerClass(...)
java.lang.StackWalker.walk(...)
```

An earlier synthetic StackWalker regression was not enough: a later real-suite
rerun still timed out for 602s with zero completed test methods. The missing
piece was the real Log4j2 `StackLocator` API path used during provider/config
bootstrap.

## Resolution

Real-JDK mode now registers a Log4j2 caller-class bridge for:

```text
org/apache/logging/log4j/util/StackLocator.getCallerClass(String,String)
org/apache/logging/log4j/util/StackLocatorUtil.getCallerClass(String,String)
```

The bridge mirrors Log4j's own `fqcn`/package scan over CratonVM's existing
frame snapshot and returns the matching class mirror directly. That bypasses
the expensive JDK `StackWalker.walk(...dropWhile...findFirst...)` stream path
for Log4j provider bootstrap while preserving the caller class shape observed on
HotSpot.

The StackWalker path also now avoids avoidable transient work:

- `SW_FRAME_CACHE` stores shared frame vectors instead of cloning the full frame
  list for every `fetchStackFrames` call.
- `capture_stack_trace(0)` no longer clones transient StackWalker/caller
  snapshots into the throwable trace map under key `0`; real throwables still
  store traces by their identity hash.

## Verification

Focused committed regression:

```powershell
$env:CARGO_TARGET_DIR='target-databuffer-stackwalker-real-20260705-001'
$env:CRATONVM_BIN=(Resolve-Path .\cratonvm-databuffer-stackwalker-real-20260705-001.exe).Path
cargo test -p cratonvm-vm --test wp1_9_stackwalker -- --nocapture
```

Result:

```text
8 passed; 0 failed; finished in 15.61s
```

Direct real Log4j 2.26.0 `StackLocatorUtil.getCallerClass(String,String)` probe
against the unique binary:

```text
caller=Log4jStackLocatorProbe$Locator
elapsedMs=35
LOG4J_STACKLOCATOR_OK
```

With `CRATONVM_DEBUG_STACKWALK=1`, the same probe emitted no `SW-DBG` lines,
confirming the Log4j caller lookup no longer enters the StackWalker native
stream path.

Closer Netty + Log4j bootstrap probe:

```text
provider=org.apache.logging.log4j.simple.internal.SimpleProvider
elapsedMs=4155
NETTY_LOG4J_BOOTSTRAP_OK
[cratonvm] main-vm run() returned Ok - VM main exiting normally
```

## Caveat

The original Spring suite command was not rerun in this checkout because
`apps/spring-suite-runner` and `apps/spring-framework` are not tracked here.
The replacement validation uses the real Log4j 2.26.0 and Netty jars from the
local Gradle cache plus committed StackWalker regressions that exercise the same
caller-resolution shape.
