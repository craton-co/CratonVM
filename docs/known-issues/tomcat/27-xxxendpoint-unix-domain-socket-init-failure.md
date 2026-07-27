# `TestXxxEndpoint.testUnixDomainSocket` — protocol handler init failure

**Status:** OPEN. Confirmed CratonVM-only regression — passes on HotSpot in
the same fixture. Fast/deterministic (~34-44s across two runs).

## Symptom

```
org.apache.catalina.LifecycleException: Protocol handler initialization failed
	at org.apache.catalina.LifecycleException.<init>(LifecycleException.java:69)
	at org.apache.catalina.connector.Connector.initInternal(Connector.java:1279)
	at org.apache.catalina.util.LifecycleBase.init(LifecycleBase.java:128)
	at org.apache.catalina.core.StandardService.initInternal(StandardService.java:543)
```

`testUnixDomainSocket` configures a Tomcat connector to bind to a Unix
domain socket path instead of a TCP host:port. Initialization fails on
CratonVM; HotSpot's connector initializes successfully with the same
config.

## Analysis

Points at a gap in CratonVM's `java.nio.channels` Unix-domain-socket support
(`UnixDomainSocketAddress`/`ServerSocketChannel.bind` over `StandardProtocolFamily.UNIX`,
JDK 16+ API) as used by Tomcat's `NioEndpoint`/connector bind path — either
the native support isn't wired up at all, or it's wired up but rejects a
config Tomcat's connector passes that real JSSE/NIO accepts. Not root-caused
to the exact missing/broken native call in this session; the full stack
trace beyond `Connector.initInternal` (truncated above) would need
capturing in a standalone repro to see the actual `Caused by:`.

## Reproduction

```powershell
.\apps\tomcat-suite-runner\run-tomcat-suite.ps1 -Category failed -RefCsv <ref> -TimeoutSec 120 -Parallel 1 -RunName xxxendpoint-repro -Exe <cratonvm.exe>
```
Check the full log (not just the truncated summary above) for the nested
`Caused by:` chain — that will point directly at the missing/broken native
call.
