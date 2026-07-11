# JULI logging subsystem — 4-class failure cluster

**Status:** OPEN. **Severity:** medium (test/logging-infra correctness, not
a crash). **HotSpot:** PASS on all four (fresh-verified).

## Summary

Four `org.apache.juli` (Tomcat's own logging façade over
`java.util.logging`) classes fail on CratonVM while passing cleanly on
HotSpot:

```
1) testOverFlow[0: overflowDropType[1]](org.apache.juli.TestAsyncFileHandlerOverflow)
java.nio.file.NoSuchFileException: output/tmp/test10841461102155016311/TestAsyncFileHandler.2026-07-11.log
2) testOverFlow[1: overflowDropType[2]](org.apache.juli.TestAsyncFileHandlerOverflow)
java.nio.file.NoSuchFileException: output/tmp/test3070043560998557358/TestAsyncFileHandler.2026-07-11.log
3) testOverFlow[2: overflowDropType[3]](org.apache.juli.TestAsyncFileHandlerOverflow)
java.nio.file.NoSuchFileException: output/tmp/test499895680452452112/TestAsyncFileHandler.2026-07-11.log
```
```
1) testCleanOnInitOneHandler(org.apache.juli.TestFileHandler)
java.lang.AssertionError
	at org.junit.Assert.fail(Assert.java:87)
	at org.junit.Assert.assertTrue(Assert.java:42)
2) <second failure in same class, same bare AssertionError signature>
```
```
1) testPerWebappHandlersIsolation(org.apache.juli.TestPerWebappJuliIntegration)
java.lang.AssertionError
	at org.junit.Assert.fail(Assert.java:87)
	at org.junit.Assert.assertTrue(Assert.java:42)
2) <second failure in same class, same bare AssertionError signature>
```
```
1) testCache(org.apache.juli.TestThreadNameCache)
org.junit.ComparisonFailure: expected:<[t-TestThreadNameCache]> but was:<[main]>
	at java.lang.AssertionError.<init>(AssertionError.java:76)
	at org.junit.ComparisonFailure.<init>(ComparisonFailure.java:37)
	at org.junit.Assert.assertEquals(Assert.java:117)
```

These four classes all exercise `FileHandler`/`AsyncFileHandler`/
per-webapp-classloader `ClassLoaderLogManager` machinery — the same
subsystem — which is why they're grouped here even though the individual
symptoms differ:

- **`TestAsyncFileHandlerOverflow`**: the async file handler's overflow-drop
  test writes to a per-test temp dir and then expects the resulting log
  file to exist (`output/tmp/<random>/TestAsyncFileHandler.<date>.log`);
  CratonVM never creates it — either the handler silently drops the file
  creation, or the async write queue never flushes/rotates to disk before
  the test asserts.
- **`TestFileHandler.testCleanOnInitOneHandler`**: bare assertion — likely
  a `FileHandler` init-time log-rotation/cleanup check (deletes files older
  than N days) that doesn't find/remove what it expects.
- **`TestPerWebappJuliIntegration.testPerWebappHandlersIsolation`**: bare
  assertion — checks that two webapps' `ClassLoaderLogManager` instances
  produce isolated handler sets; something about that isolation isn't
  holding.
- **`TestThreadNameCache.testCache`**: the clearest signal of the four —
  Tomcat's `ThreadNameCache` is supposed to record the *current* thread's
  name (expected `t-TestThreadNameCache`, a named worker thread the test
  spins up) but CratonVM returns `main` instead, meaning the cache is
  either not being populated per-thread correctly, or is reading a stale/
  wrong-thread value (possibly a `ThreadLocal` scoping bug, or the named
  thread's name isn't propagating into whatever CratonVM backs
  `Thread.currentThread().getName()` with at the point this cache reads it).

Found via a fresh Linux rerun (dev commit `2335765e`, real JDK, JIT on,
300s timeout) as part of a large non-passed-class sweep on 2026-07-11.
Verified via a fresh HotSpot run on the same host at the same time: all
four PASS on HotSpot, confirming these are CratonVM-specific, not a stale
baseline or host/fixture artifact (this session found ~60 other
apparent-CratonVM-failures in the same sweep that turned out to also fail
on a fresh HotSpot run — these four are not among them).

## Reproduction

```bash
JH=/home/victor/jdk25
CP=$(cat /data/data/apps/tomcat/.suite/cp-linux-fixed.txt)
"$CRATONVM_EXE" --java-home "$JH" -Xmx2g -cp "$CP" org.junit.runner.JUnitCore \
  org.apache.juli.TestAsyncFileHandlerOverflow \
  org.apache.juli.TestFileHandler \
  org.apache.juli.TestPerWebappJuliIntegration \
  org.apache.juli.TestThreadNameCache
```

## Recommendation

Start with `TestThreadNameCache` — it has the most concrete, actionable
signal (`main` instead of the spun-up worker thread's name). Trace where
Tomcat's `ThreadNameCache` reads the current thread's name and compare
against how CratonVM threads created via `Thread.start()` propagate their
name internally; this may point at the same root cause behind the other
three (if the logging subsystem's per-thread/per-handler state is keyed off
a thread-identity mechanism that CratonVM doesn't fully honor, that would
explain the FileHandler/AsyncFileHandler/PerWebapp isolation failures too).
