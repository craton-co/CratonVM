# TestStandardHostValve / TestStuckThreadDetectionValve - fixed valve-layer failures

**Status:** FIXED (2026-07-12). **Severity:** medium. **HotSpot:** PASS.

## Symptoms

- `TestStandardHostValve.testIncompleteResponse` made a bare assertion after an incomplete HTTP response.
- `TestStuckThreadDetectionValve.testInterruption` failed with a null `String` at `startsWith()` after the stuck-thread valve interrupted its servlet.

## Resolution

Two independent runtime defects were corrected:

1. The real-JDK `HttpURLConnection` bridge now propagates a premature response-head close as `IOException`, rather than converting it to status `-1`. This restores Tomcat's expected incomplete-response handling.
2. `Thread.sleep` now clears the VM interrupt flag unconditionally after its polling loop. Previously, the `interrupted || clear()` expression short-circuited when an interrupt arrived during sleep, leaving the flag set. Tomcat's `NioChannel.checkInterruptStatus()` then consumed the stale interrupt before it wrote the servlet response and returned `SocketState.CLOSED`.

The validation also exposed a general lambda-cache correctness gap: cached bytecode method references could bypass registered native methods. Native targets now bypass that cache and use ordinary dispatch, restoring the native implementation for `ReentrantReadWriteLock.readLock()` used by Tomcat's keyed lock.

## Verification

Remote Linux host, real JDK 25, JIT enabled, using the isolated binary
`/data/data/cratonvm-tomcat-standardhostvalve-stuckthread-20260712-interrupt-clear`:

```bash
JH=/home/victor/jdk25
CP=$(cat /data/data/apps/tomcat/.suite/cp-linux-fixed.txt)
"$CRATONVM_EXE" --java-home "$JH" -Xmx2g -cp "$CP" org.junit.runner.JUnitCore org.apache.catalina.core.TestStandardHostValve org.apache.catalina.valves.TestStuckThreadDetectionValve
```

- Full affected set: `OK (10 tests)` in 20.411 seconds.
- Independent repeat of `TestStuckThreadDetectionValve`: `OK (2 tests)` in 25.187 seconds.

The issue is retired from `docs/known-issues`.
