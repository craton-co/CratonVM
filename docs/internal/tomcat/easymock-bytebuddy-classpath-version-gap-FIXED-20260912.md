# 8 classes failed on `NoClassDefFoundError: net/bytebuddy/TypeCache$WithInlineExpunction` — a classpath version gap — FIXED 2026-09-12

## Status
**FIXED (fixture), never a CratonVM bug.** Moved from
`docs/known-issues/tomcat/` 2026-09-12. The suite classpath now carries the
byte-buddy and objenesis versions Tomcat itself pins, and all 8 classes PASS
on HotSpot and on CratonVM.

| class | HotSpot | CratonVM |
|---|---|---|
| `org.apache.catalina.realm.TestJNDIRealm` | OK (4) | OK (4) |
| `org.apache.catalina.session.TestPersistentManager` | OK (2) | OK (2) |
| `org.apache.catalina.startup.TestWebappServiceLoader` | OK (7) | OK (7) |
| `org.apache.catalina.valves.TestCrawlerSessionManagerValve` | OK (5) | OK (5) |
| `org.apache.catalina.valves.TestLoadBalancerDrainingValve` | OK (192) | OK (192) |
| `org.apache.catalina.valves.TestSSLValve` | OK (19) | OK (19) |
| `org.apache.catalina.connector.TestRequest` | OK (40) | OK (40) |
| `org.apache.jasper.servlet.TestTldScanner` | OK (3) | OK (3) |

Measured 2026-09-12, local Windows box, real JDK 25, one class per process with
the suite's JVM arguments.

## What was reported

All 8 failed the moment they created a class mock:

```
java.lang.NoClassDefFoundError: net/bytebuddy/TypeCache$WithInlineExpunction
	at org.easymock.internal.ClassProxyFactory.<init>(ClassProxyFactory.java:137)
	at org.easymock.internal.MocksControl.getClassProxyFactory(MocksControl.java:163)
	at org.easymock.internal.MocksControl.createMock(MocksControl.java:107)
```

identically on HotSpot. `cp.txt` carried `byte-buddy-1.14.12.jar`, which ships
no `TypeCache$WithInlineExpunction`; EasyMock 5.6.0 needs a newer one.

## Root cause and fix

`Build-Classpath` matched `byte-buddy-1.*.jar` and `objenesis-3.*.jar` by glob
and took whichever file a directory walk met first — nothing tied the choice
to the version Tomcat builds against (`bytebuddy.version=1.18.8`,
`objenesis.version=3.5` in `build.properties.default`). The same classpath
entry was also one of the six dead Gradle-cache paths in
`cp-txt-stale-gradle-module-cache-paths-FIXED-20260912.md`, whose fix replaced
the glob walk: the harness now reads Tomcat's pins and checksums, copies the
exact jars into `apps/tomcat/.suite/lib/pinned/`, and checks the classpath
before every run.

## Correction to `run-tomcat-suite.md`

§6 of the harness notes said EasyMock 5.6.0 class-mocking cannot work on JDK 25
on HotSpot "under any flag combination", so these classes were expected red in
every HotSpot control run. That was measured with byte-buddy 1.14.12. With the
version Tomcat pins, all 8 are green on HotSpot too, and the note is replaced.
