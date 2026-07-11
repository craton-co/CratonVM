# TestTomcatStandalone — missing server version line in VersionLoggerListener output

**Status:** OPEN. **Severity:** low-medium. **HotSpot:** PASS (fresh-verified).

## Summary

`org.apache.catalina.startup.TestTomcatStandalone.testStandalone` fails:
```
1) testStandalone(org.apache.catalina.startup.TestTomcatStandalone)
java.lang.AssertionError: Missing server version line in VersionLoggerListener output.
	at java.lang.AssertionError.<init>(AssertionError.java:76)
	at org.junit.Assert.fail(Assert.java:89)
	at org.junit.Assert.assertTrue(Assert.java:42)
	at org.apache.catalina.startup.TestTomcatStandalone.assertVersionLoggerListenerOutput(TestTomcatStandalone.java:290)
```
`VersionLoggerListener` is a Tomcat `LifecycleListener` that logs a fixed
set of startup banner lines (server version, JVM version, OS info, etc.)
via JULI when the server starts. The test captures log output and asserts
a line matching the server-version banner is present; on CratonVM it isn't
found. Likely candidates: the listener never fires (a lifecycle-event
wiring gap specific to the `Tomcat.start()`-driven "standalone" embedding
path this test exercises, as opposed to the more common
`TomcatBaseTest`/webapp-fixture startup path most other tests use), or it
fires but the log capture mechanism this specific test uses doesn't see
CratonVM's JULI output.

Found via a fresh Linux rerun (dev commit `2335765e`, real JDK, JIT on,
300s timeout) on 2026-07-11. Verified via a fresh same-session HotSpot run:
PASSES on HotSpot.

## Reproduction

```bash
JH=/home/victor/jdk25
CP=$(cat /data/data/apps/tomcat/.suite/cp-linux-fixed.txt)
"$CRATONVM_EXE" --java-home "$JH" -Xmx2g -cp "$CP" org.junit.runner.JUnitCore \
  org.apache.catalina.startup.TestTomcatStandalone
```

## Recommendation

Read `TestTomcatStandalone.java` around line 290 and its `testStandalone`
setup to see exactly how it captures log output (likely a custom JULI
`Handler` attached before `Tomcat.start()`) and whether
`VersionLoggerListener` is registered on the `Tomcat` instance this test
constructs directly (rather than via the shared `TomcatBaseTest` fixture
most other classes use) — this is a good candidate to cross-check against
the JULI logging cluster in
[juli-logging-subsystem-cluster.md](juli-logging-subsystem-cluster.md),
since both involve JULI handler/output visibility gaps, though the
call paths differ enough that they may be unrelated.
