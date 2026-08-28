# 8 classes fail on `NoClassDefFoundError: net/bytebuddy/TypeCache$WithInlineExpunction` — a classpath version gap, not a CratonVM bug

## Status
**Confirmed NOT a CratonVM bug.** Fails identically on HotSpot with the
identical classpath.

## The classes

`TestJNDIRealm`, `TestPersistentManager`, `TestWebappServiceLoader`,
`TestCrawlerSessionManagerValve`, `TestLoadBalancerDrainingValve`,
`TestSSLValve`, `TestRequest` (coyote), `TestTldScanner` — all fail the moment
they call `EasyMock.mock`/`niceMock`/`createMock`/`createNiceMock`:

```
java.lang.NoClassDefFoundError: net/bytebuddy/TypeCache$WithInlineExpunction
	at org.easymock.internal.ClassProxyFactory.<init>(ClassProxyFactory.java:137)
	at org.easymock.internal.MocksControl.getClassProxyFactory(MocksControl.java:163)
	at org.easymock.internal.MocksControl.createMock(MocksControl.java:107)
```

## Confirmed real gap, not a loading defect

The fixture's classpath (`apps/tomcat/.suite/cp.txt`) carries exactly one
byte-buddy jar:
```
byte-buddy-1.14.12.jar
```
`jar tf byte-buddy-1.14.12.jar | grep -i typecache` returns **nothing** — this
jar genuinely does not contain any `TypeCache` class, let alone the
`$WithInlineExpunction` nested one. EasyMock's `ClassProxyFactory` needs a
newer byte-buddy than what's on the classpath. HotSpot cross-check, identical
setup: `FAIL` in the same way.

## Fix

Not attempted — this is a fixture/dependency-resolution issue
(`apps/tomcat/.suite/cp.txt` generation, or whatever Gradle/Maven coordinate
pins byte-buddy to 1.14.12 for this classpath) rather than a CratonVM one.
Bumping the byte-buddy jar on this classpath to a version that ships
`TypeCache$WithInlineExpunction` (present since byte-buddy ~1.15) should
recover all 8 classes — not verified.

## Repro

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm hotspot -Category all -Start 199 -Count 1 -RunName repro -TimeoutSec 60
# org.apache.catalina.realm.TestJNDIRealm is index 199 in .suite\all-tests.txt;
# any of the 8 classes above reproduces the same NoClassDefFoundError
```
