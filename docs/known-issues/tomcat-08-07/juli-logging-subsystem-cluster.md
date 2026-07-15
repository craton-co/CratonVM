# JULI logging subsystem — resolved 4-class cluster

**Status (2026-07-15): PARTIALLY REOPENED.** 3 of 4 classes
(`TestAsyncFileHandlerOverflow`, `TestFileHandler`, `TestThreadNameCache`)
are confirmed PASS on a fresh `dev` @ `f23a3f42a` rerun — those fixes hold.
`TestPerWebappJuliIntegration.testPerWebappHandlersIsolation` still
reproduces the exact original bare-assertion failure:
```
1) testPerWebappHandlersIsolation(org.apache.juli.TestPerWebappJuliIntegration)
java.lang.AssertionError
	at org.junit.Assert.assertNotNull(Assert.java:713)
	at org.apache.juli.TestPerWebappJuliIntegration.testPerWebappHandlersIsolation(TestPerWebappJuliIntegration.java:83)
```
Not confounded by the known Windows test-fixture gap (no `StandardRoot`/
`child container failed` marker in this run's log) — this looks like a
genuine, still-open residual distinct from whatever fixed the other 3.

**Original status:** FIXED on 2026-07-11. **Severity:** medium (test/logging
infrastructure correctness; no crash).

## Resolution

The failure cluster had four independent runtime gaps on the real-JDK JUL
path:

- `ThreadMXBean` returned an unnamed/basic `ThreadInfo`, so JULI's thread-name
  cache observed `main` instead of the worker's registered name.
- Native JUL dispatch printed intercepted `Logger` calls but did not fan them
  out to attached JULI handlers; `LogRecord` and `Handler` bridge state was
  incomplete as well.
- `AsyncFileHandler`'s concrete executor did not use the real
  `ThreadPoolExecutor` state machine or wait for termination, leaving its
  overflow writes unflushed.
- `FileHandler.clean()` was queued through that path rather than performing
  its bounded expired-file deletion reliably during initialization.

The runtime now preserves named `ThreadInfo`, handler/formatter/log-record
state and handler fan-out, routes the JULI executor through the real
`ThreadPoolExecutor` implementation, and performs the bounded JULI cleanup
operation deterministically.

## Verification

Fresh real-JDK CratonVM run on the Azure Linux host, with JIT enabled:

```bash
JH=/home/victor/jdk25
CP=$(cat /data/data/apps/tomcat/.suite/cp-linux-fixed.txt)
"$CRATONVM_EXE" --java-home "$JH" -Xmx2g -cp "$CP" org.junit.runner.JUnitCore \
  org.apache.juli.TestAsyncFileHandlerOverflow \
  org.apache.juli.TestFileHandler \
  org.apache.juli.TestPerWebappJuliIntegration \
  org.apache.juli.TestThreadNameCache
```

Result: `OK (10 tests)` in 3.139 seconds on 2026-07-11.

This record was moved from `docs/known-issues/tomcat-08-07/` after that full
cluster rerun passed.
