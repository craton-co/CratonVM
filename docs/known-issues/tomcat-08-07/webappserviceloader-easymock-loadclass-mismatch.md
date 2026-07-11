# TestWebappServiceLoader — EasyMock ClassLoader.loadClass call-count mismatch

**Status:** OPEN. **Severity:** low. **HotSpot:** PASS (fresh-verified).

## Summary

`org.apache.catalina.startup.TestWebappServiceLoader.testNoInitializersFound`
fails:
```
1) testNoInitializersFound(org.apache.catalina.startup.TestWebappServiceLoader)
java.lang.AssertionError:
  Unexpected method call Mock named org.easymock.mocks.ClassLoader$$$EasyMock$1@23d59 -> ClassLoader.loadClass("org.apache.jasper.servlet.JasperInitializer"):
    EasyMock for class java.lang.ClassLoader -> ClassLoader.getResources("META-INF/services/jakarta.servlet.ServletContainerInitializer"): expected: 1, actual: 1
```
This test mocks a `ClassLoader` and verifies Tomcat's
`WebappServiceLoader` (the `ServiceLoader`-based mechanism that finds
`ServletContainerInitializer` implementations via
`META-INF/services/jakarta.servlet.ServletContainerInitializer`) makes a
specific, exact sequence of calls against it — in this "no initializers
found" case, `getResources(...)` is expected to be called exactly once and
nothing else. EasyMock's failure message is confusing at first glance (it
shows `expected: 1, actual: 1` for the call that DID match, while the real
complaint is the *unexpected* extra `loadClass("...JasperInitializer")`
call) — the actual bug is that CratonVM's `WebappServiceLoader` path calls
`loadClass()` on the mock classloader when HotSpot's does not, meaning
CratonVM is doing extra classloading work (or doing it in a different order/
place) that this strict mock doesn't expect for the "no initializers"
scenario.

Found via a fresh Linux rerun (dev commit `2335765e`, real JDK, JIT on,
300s timeout) on 2026-07-11. Verified via a fresh same-session HotSpot run:
PASSES on HotSpot.

## Reproduction

```bash
JH=/home/victor/jdk25
CP=$(cat /data/data/apps/tomcat/.suite/cp-linux-fixed.txt)
"$CRATONVM_EXE" --java-home "$JH" -Xmx2g -cp "$CP" org.junit.runner.JUnitCore \
  org.apache.catalina.startup.TestWebappServiceLoader
```

## Recommendation

Read `org.apache.catalina.startup.WebappServiceLoader`'s implementation
alongside `TestWebappServiceLoader.testNoInitializersFound`'s mock setup —
identify what triggers the extra `loadClass("org.apache.jasper.servlet.JasperInitializer")`
call on CratonVM. This has the flavor of a `ServiceLoader`/`Class.forName`
internal implementation difference (e.g. CratonVM's `ServiceLoader` eagerly
resolving/loading a provider class name it read from the services file,
where HotSpot's defers loading until iteration actually reaches that
provider) rather than a Tomcat-level logic bug, since the test only fails
on the "no initializers" empty-services-file case.
