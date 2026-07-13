# Silent 1200s hangs with no diagnostic signature (3 classes)

**Status:** OPEN, needs deeper investigation. **Severity:** medium-high
(indefinite hang). **HotSpot:** PASS on all 3 (fresh-verified).

## Summary

Three classes HANG at the full 1200s timeout without printing any
error/warning in stdout or stderr beyond normal startup — unlike the
[STW cross-thread JIT takeover cluster](stw-crossthread-jit-takeover-hang-cluster.md),
these three show **no** `STW cross-thread JIT takeover` warning or any
other diagnostic before the process is killed:

- `org.apache.catalina.startup.TestContextConfig` — log stops right after
  `INFO [org.apache.catalina.startup.ContextConfig] No global web.xml
  found`, mid-`testBug54262`.
- `org.apache.catalina.connector.TestResponsePerformance` — log stops
  right after `INFO [...] Starting test case [testToAbsolutePerformance]`.
  Note: a related earlier investigation this session found this same
  class SIGABRT-crashes on Linux with `OutOfMemoryError: young gen
  exhausted` at the default 2g heap — this Windows run shows a plain hang
  instead, not a crash. Possibly the same underlying GC-pressure issue
  manifesting differently per-platform (Windows perhaps handles the OOM
  differently, e.g. blocking allocation retry instead of aborting), or two
  separate issues. Worth checking with a bumped heap (`-Xmx8g`+) per the
  Linux investigation's approach before assuming this is a genuine
  CratonVM hang bug rather than a heap-sizing artifact.
- `org.apache.jasper.compiler.TestValidator` — log stops immediately after
  `JUnit version 4.13.2`, before any test-case `INFO` line even prints.

## Reproduction

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName silenthang `
  -Start <idx> -Count 1 -TimeoutSec 300 -Parallel 1
# org.apache.catalina.startup.TestContextConfig
# org.apache.catalina.connector.TestResponsePerformance
# org.apache.jasper.compiler.TestValidator
```

## Recommendation

Before deep investigation, rule out the heap-sizing false-positive pattern
already confirmed for `TestResponsePerformance` on Linux (bump `-Xmx` to
8-12g and see if it completes cleanly instead of hanging) — apply the same
check to `TestContextConfig` and `TestValidator` since neither shows any
diagnostic output, consistent with a resource-starvation stall rather than
a logic deadlock. If they still hang at a generous heap, get a thread dump
or attach a debugger at the hang point to see where each class's threads
are actually blocked — with zero log signal, a live stack trace is the
only way to distinguish "waiting on I/O", "stuck in GC", "stuck in a JIT
compile", or "genuine deadlock" without further guessing.
