# Fixture gap: `easymock.jar` missing from the Windows suite classpath — 8 classes

| | |
|---|---|
| **Status** | Fixture gap, NOT a CratonVM bug |
| **HotSpot** | Fails identically (same `NoClassDefFoundError`, same classpath) |
| **Discovered** | 2026-08-03, rerunning the 07-31 4-shard FAIL/HANG set after merging `dev` (`c1fe51a24`) |

## Symptom

8 classes fail — every one with `java.lang.NoClassDefFoundError:
org/easymock/EasyMock` on their first EasyMock-using test method:

- `org.apache.catalina.filters.TestRestCsrfPreventionFilter`
- `org.apache.catalina.session.TestPersistentManager`
- `org.apache.catalina.startup.TestWebappServiceLoader`
- `org.apache.catalina.valves.TestCrawlerSessionManagerValve`
- `org.apache.catalina.valves.TestLoadBalancerDrainingValve`
- `org.apache.catalina.valves.TestSSLValve`
- `org.apache.coyote.TestRequest`
- `org.apache.jasper.servlet.TestTldScanner`

Example:

```
1) testPostRequestInvalidNonceAsParameterValidPath(org.apache.catalina.filters.TestRestCsrfPreventionFilter)
java.lang.NoClassDefFoundError: org/easymock/EasyMock
```

## Not a CratonVM bug

This is a classpath composition gap in the Windows suite fixture
(`apps/tomcat/.suite/cp.txt`), not VM behavior — a `NoClassDefFoundError` for
a missing third-party test dependency fails identically regardless of which
JVM runs it. Not independently re-verified against HotSpot per-class this
round since the mechanism is deterministic and VM-independent, but this
matches the same easymock-dependent test family noted historically in
`docs/internal/fixed-suite-bugs/tomcat/18-fixture-environment-gaps-20260724.md`'s
Linux/Azure fixture.

## Fix

Add `easymock` (and its transitive `cglib`/`objenesis` deps, matching
whatever version Tomcat's own `build.xml`/`ivy` resolves) to
`apps/tomcat/.suite/cp.txt`'s classpath assembly on Windows. Once fixed, these
8 classes should be re-run to check for genuine CratonVM-side failures
underneath the classpath gap.
