# TestManagerWebapp — bare assertion failures in deploy/servlet-listing tests

**Status:** OPEN (root cause #1 FIXED, two deeper residuals discovered — see below).
**Severity:** medium. **HotSpot:** PASS.

## 2026-07-10 update — root cause #1 found and fixed; two new residuals uncovered

Root-caused and fixed the dominant blocker: **every single HTTP request into the
manager webapp** (not just deploy) was failing with a bare `AssertionError`
because it got a `500` instead of the expected status. The `manager` webapp's
`META-INF/context.xml` configures
`org.apache.catalina.valves.RemoteCIDRValve` (`allow="127.0.0.0/8,::1/128"`),
which runs on *every* request and calls `request.getRequest().getRemoteAddr()`.

**Root cause:** `native-builtins/src/phases_early.rs`'s
`register_phase52_inet_socket_address` registers a synthetic native override
for `InetSocketAddress(String, int)` (`java/net/InetSocketAddress.<init>`)
that unconditionally passed `addr=None` — it never attempted to resolve the
hostname at all, so **every** string-constructed `InetSocketAddress` (even a
literal IP like `127.0.0.1`) came out permanently `isUnresolved()==true`. Its
`toString()` still printed a normal-looking `host:port`, which is why this
went unnoticed — only callers that extract the `InetAddress` (e.g.
`sun.nio.ch.SocketAdaptor.getInetAddress()`, which Tomcat's NIO endpoint uses
via `.socket().getInetAddress()` to populate `Request.remoteAddr`) saw the
gap. `RemoteCIDRValve.invoke()` then did:
```java
property = request.getRequest().getRemoteAddr();   // null
...
process(property, ...) -> isAllowed(property) -> property.indexOf(';')  // NPE
```
producing `NullPointerException: Cannot invoke "String.indexOf(int)" because
"property" is null` on **every** request, logged as `Exception Processing
[/manager/...]` and surfaced to the client as a bare `500`. Since
`testDeploy`/`testServlets`/`testBug57700` all use bare `assertTrue`/
`assertEquals` without messages against these responses, the failures showed
up exactly as the "no message, no clue" bare `AssertionError`s this doc
originally described.

**Fix:** `native-builtins/src/phases_early.rs`'s
`register_phase52_inet_socket_address`'s `<init>(Ljava/lang/String;I)V`
handler now attempts real resolution before falling back to unresolved,
matching real `InetSocketAddress(String, int)` semantics. This exact bug was
independently found and fixed in a concurrent session the same day, landed
as dev commit `0e8c0df4` ("Fix InetSocketAddress(String,int) never resolving
hostname to an address") via `net_phase_e::resolve_host_external` — see
[`form-authenticator-cookie-session-bare-assertion.md`](form-authenticator-cookie-session-bare-assertion.md)
for that investigation's parallel write-up (same root cause, reached via
`TestFormAuthenticatorA/B/C` instead of `TestManagerWebapp`). Verified here
with isolated probes (`new InetSocketAddress("127.0.0.1", N)` now correctly
resolves; `SocketChannel.socket().getInetAddress()` off an accepted server
socket now returns the real peer address instead of `null`) and the existing
`vm/tests/wave3_b2_dispatch.rs` regression tests (`getPort()` round-trip
through this exact constructor) still pass.

**Two new residuals surfaced once the NPE stopped masking everything else**
(both classes now get past the connectivity failure and hit *different*,
pre-existing JMX/Modeler infrastructure gaps):

1. **`testDeploy` / `testBug57700` — `BaseModelMBean` JMX reflective-invoke
   dispatch gap.** `ManagerServlet.tryAddServiced(String)` calls
   `mBeanServer.invoke(deployerObjectName, "tryAddServiced", params,
   signature)` — a dynamic JMX operation invocation that should reflect
   through Tomcat's Modeler (`BaseModelMBean`, a `DynamicMBean` wrapping the
   real `HostConfig`/Deployer resource) to the *wrapped resource's* method.
   Under CratonVM this throws
   `NoSuchMethodError: org/apache/tomcat/util/modeler/BaseModelMBean.tryAddServiced(Ljava/lang/Object;)Ljava/lang/Object;`
   — i.e. something in CratonVM's JMX invoke-dispatch bridge looks for the
   operation directly on the `BaseModelMBean` wrapper class instead of
   reflecting into the managed resource. This makes the manager's deploy
   operation report failure (`testDeploy`'s
   `assertTrue(getResponseBody().contains("OK - "))` at
   `TestManagerWebapp.java:309` fails) and very likely explains
   `testBug57700`'s `expected:<302> but was:<404>` at line 600 too (the app
   silently fails to deploy, so its context is never available to redirect).

2. **`testServlets` — `ManagementFactory.getPlatformMBeanServer()` returns a
   non-concrete `MBeanServer`.** `StatusManagerServlet.init()` calls
   `Registry.getRegistry(null).getMBeanServer()`, which ultimately reaches
   `ManagementFactory.getPlatformMBeanServer()`. An isolated probe
   (`MBeanServer server = ManagementFactory.getPlatformMBeanServer();
   server.getClass()`) shows the returned object's class is literally
   `interface javax.management.MBeanServer` — not a concrete
   `com.sun.jmx.mbeanserver.JmxMBeanServer` — so any interface method
   CratonVM hasn't specifically special-cased throws
   `AbstractMethodError: ... has no Code attribute`, observed concretely on
   `addNotificationListener(ObjectName, NotificationListener,
   NotificationFilter, Object)`. **Both of CratonVM's known synthetic
   `MBeanServer` stand-ins were ruled out** by direct instrumentation:
   `config.use_synthetic_jdk` is confirmed `false` for this (real-JDK)
   invocation, and neither `register_management_factory_platform_server_stub`
   nor `register_mbean_server_factory_synthetic` (both gated to
   synthetic-JDK-only, per the existing KAFKA-MBEAN design note in
   `native-builtins/src/jmx.rs`) fire at all — confirmed their entry prints
   never appear. So the wrong-typed object is being produced somewhere
   *inside* the real bytecode chain (`MBeanServerFactory.createMBeanServer()`
   → real `JmxMBeanServer` construction), not by a leaking synthetic
   override. Root mechanism not yet pinned down; needs a fresh, dedicated
   investigation (probably tracing `MBeanServerFactory.createMBeanServer()`'s
   real bytecode step by step under CratonVM to find where a concrete
   allocation silently degrades to the bare interface type). This crashes
   `StatusManagerServlet`'s `init()`, which turns the *first* authenticated
   `/manager/html` request into a `500`
   (`testServlets`'s `assertEquals(200, ...)` at line 131).

**Net:** the doc stays OPEN — `testDeploy`/`testServlets` still fail — but the
original "no leads, bare AssertionError" state is gone. The remaining work is
two distinct, well-characterized JMX/Modeler gaps (above), not a mystery.

## Original summary

`org.apache.catalina.manager.TestManagerWebapp` fails `testDeploy` and
`testServlets` with bare `AssertionError` (no message):
```
1) testDeploy(org.apache.catalina.manager.TestManagerWebapp)
java.lang.AssertionError
2) testServlets(org.apache.catalina.manager.TestManagerWebapp)
java.lang.AssertionError
```
This class drives the Tomcat Manager webapp's deploy/list-servlets HTTP
endpoints. (The original doc's guess that this might be the *same*
underlying issue as the Group 04 deploy-throughput wall was checked and
**refuted** — reproduction here completes well within 60s wall-clock, not a
timeout/hang, and the true cause was the unrelated `RemoteCIDRValve`/
`InetSocketAddress` bug above.)

Found via a full Windows Tomcat suite rerun (real JDK, JIT on, 1500s
timeout, dev commit range `33bef88d`..`0d8fb610`, 2026-07-07/08).

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName mgrwebapp `
  -Start <idx> -Count 1 -TimeoutSec 60 -Parallel 1
# org.apache.catalina.manager.TestManagerWebapp
```

Linux/Azure-host equivalent (no `tomcat-suite-runner` checked in for this
platform — harness assembled ad hoc under `/data/data/apps/tomcat` +
`/data/data/tomcat-build-libs`, classpath in `.suite/cp.txt`; the `manager`
webapp itself is not part of that harness's `output/build/webapps` — stage
it from `/tmp/tomcat-src-ref/webapps/manager` and point
`-Dtomcat.test.tomcatbuild=<dir>` at a directory containing both `examples`
and `manager`):
```bash
cd /data/data/apps/tomcat
CP=$(cat .suite/cp.txt)
cratonvm --java-home /home/victor/jdk25 -Xmx2g \
  -Dtomcat.test.tomcatbuild=<dir-with-examples-and-manager-webapps> \
  -cp "$CP" org.junit.runner.JUnitCore org.apache.catalina.manager.TestManagerWebapp
```

## Recommendation

1. Investigate the `BaseModelMBean` JMX reflective-invoke gap (residual #1
   above) — likely in whatever CratonVM code backs `MBeanServer.invoke(...)`
   for a `DynamicMBean`-wrapped resource; it should reflect into the wrapped
   resource object, not look for the operation on the wrapper class itself.
2. Investigate why `MBeanServerFactory.createMBeanServer()`'s real bytecode
   path produces a bare-interface-typed `MBeanServer` under CratonVM
   (residual #2 above) — trace step by step, since both known synthetic
   short-circuits are confirmed inactive for this path.
