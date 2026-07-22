# Bug C — embedded `TomcatBaseTest` HTTP tests hang (NIO port plumbing + connector lifecycle)

**Severity:** High by count (dominant hang cluster — most
`org.apache.catalina.*` / `org.apache.coyote.*` / `jakarta.servlet.http.*`
tests extend `TomcatBaseTest`, start an embedded server, and drive it via
`SimpleHttpClient` at `getPort()`).
**Status on CratonVM:** hang. **HotSpot:** pass.
**Binary:** worktree `cratonvm-acs.exe` (dev `c8f3bb3a` + Bug A + Bug C NIO fixes)

This bug is **independent of and downstream from Bug A**. Bug A let the embedded
server *start*; the tests then hang at the HTTP request stage.

## Symptom

`TomcatBaseTest` server tests print test-case banners then hang (120 s
timeout). `SimpleHttpClient.connect` blocks; the main thread parks in CratonVM
native networking code (watchdog: "main thread is in native (Rust) code").

## Root causes — two FIXED, one residual

Investigated with minimal embedded-Tomcat + raw-`ServerSocketChannel` repros
(`apps/tomcat/.tooling/Mini*.java`, `Ssc*.java`).

### (C1) `java.net.Socket.connect(new InetSocketAddress(host, port))` → port 0  ✅ FIXED
`SimpleHttpClient` connects with `new Socket()` + `connect(InetSocketAddress)`.
CratonVM's `read_inet_socket_address` read the raw `ISA_PORT` slot (1) first and
accepted a stale `Int(0)` from a real-JDK `InetSocketAddress` (whose only
declared field is `holder` at slot 0; the port lives at `holder.port`). So the
client connected to `host:0` → `ConnectException: host:0 (os error 10049)`.
Fixed (commit `18fa93f6`): when slot 0 is a real holder object, read
`holder.port`. Verified: `connect` now targets the real port.

### (C2) `ServerSocketChannel.getLocalAddress()` drops the port  ✅ FIXED
`NioEndpoint.getLocalPort()` does
`((InetSocketAddress) serverSock.getLocalAddress()).getPort()`.
`ssc_local_address` built a flat 2-slot `InetSocketAddress` whose null `holder`
made the real `getPort()` bytecode return garbage → `getLocalPort()` = -1.
Fixed (commit `18fa93f6`): build it via the real `(String,int)` constructor.
Verified end-to-end at the NIO level — a raw round-trip
`ServerSocketChannel.open/bind(0)/getLocalAddress().getPort()/accept` +
`Socket.connect` now returns the live ephemeral port and exchanges data on both
fixed and ephemeral ports (HotSpot-equivalent):

```
CRATON ACS: bound; ssc.getLocalAddress=0.0.0.0/0.0.0.0:58469  derived getPort=58469  client connected=true  DONE
```

### (C3) `Tomcat.start()` was a synthetic no-op stub  ✅ FIXED
After C1/C2 the connector still didn't bind: after `tomcat.start()` the entire
lifecycle stayed in state **`NEW`** (server/service/engine/connector all
unstarted). Root cause: `net_phase_e.rs` registered
`org/apache/catalina/startup/Tomcat.start()V` — plus base
`StandardContext.initInternal/startInternal` and `ContainerBase$StartChild.call`
— as **no-ops** (a Spring-Boot-era shim, forbidden by
`memory/feedback_no_synthetic_stubs.md`). `Tomcat.start()` is exactly what
`TomcatBaseTest`/standalone Tomcat call, so the server never started.
Fixed (commit `1becb78`): removed the base-Catalina no-ops (kept the Spring-Boot
`TomcatEmbeddedContext`/`TomcatWebServer` subclass shims, so Spring Boot is
unaffected). The real lifecycle runs.

### (C4) `URLClassLoader.getURLs()` NPE (null `ucp`) failed the webapp loader  ✅ FIXED
Removing the no-op exposed the masked failure: `WebappLoader.startInternal` →
`URLClassLoader.getURLs()` → `NullPointerException: Cannot invoke getURLs on
null`. CratonVM's `URLClassLoader.<init>` natives never set the inherited `ucp`
(`URLClassPath`) field; the real `getURLs()`/`findResource()` bytecode
dereferences it. Fixed (commit `1becb78`): `init_urlclassloader_fields` now
installs a bare `URLClassPath` instance (CratonVM's shim natives make
`getURLs()`→empty / `findResource()`→null), mirroring the existing `closeables`
fix. Class loading is unchanged (served from the global dynamic classpath).

## Result  ✅ Embedded Tomcat serves HTTP

With C1–C4 fixed, embedded Tomcat starts and serves:

```
CRATON final: [mini] getLocalPort=61132
              [resp] HTTP/1.1 200   Content-Length: 5   -> HELLO
```

`StandardContext` tests that hung before now complete; `TestTomcat` runs through
26+ test methods (vs hanging at method 0 before).

## Remaining caveat (not a bug — performance)

Large multi-method server test classes (e.g. `TestTomcat`, `TestHttpServlet`,
`TestConnector`) are **slow** under the interpreter — each method starts/stops a
full embedded server (~6 s each on CratonVM), so a 20–40-method class can exceed
a 120 s per-class timeout and still be classified HANG even though it is
progressing normally (not stuck). This is a throughput limitation, distinct from
the original defect (a silent no-op that never served). A longer per-test
timeout (or JIT) lets them complete.

## Reproduction

```
# Embedded server now serves: cratonvm.exe -cp .;<cp> MiniRaw2  -> HTTP/1.1 200, HELLO
# Single-server tests pass; large multi-method classes are slow (timeout, not hung)
```
