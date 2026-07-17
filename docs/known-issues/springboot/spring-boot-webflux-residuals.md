# `module/spring-boot-webflux`: two unrelated residuals (NPE + unconfirmed HANG)

**Status: OPEN — found 2026-07-17**

(The module's third non-passing class in this rerun,
`WebFluxObservationAutoConfigurationTests`, plus
`DefaultErrorWebExceptionHandlerIntegrationTests`, are the
`CapturedOutput`-empty cluster — folded into
[`conditionevaluationreport-capturedoutput-empty-cluster.md`](conditionevaluationreport-capturedoutput-empty-cluster.md)
instead of repeated here.)

## Issue A — `RecordableServerHttpRequestTests.getRemoteAddress()`: NPE, `InetSocketAddress.getAddress()` returns null

| Class | tests failed/total |
|---|---:|
| `RecordableServerHttpRequestTests` | 1/5 |

```
JUnit Jupiter:RecordableServerHttpRequestTests:getRemoteAddress()
    => java.lang.NullPointerException: Cannot invoke "java.net.InetAddress.toString()" because the return value of "java.net.InetSocketAddress.getAddress()" is null
       org.springframework.boot.webflux.actuate.web.exchanges.RecordableServerHttpRequestTests.getRemoteAddress(RecordableServerHttpRequestTests.java:91)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-webflux.org.springframework.boot.webflux.actuate.web.exchanges.RecordableSe-ab29f0cb3cbc.out.log`
(finishes in 1.6s, 4/5 tests pass — an isolated, fast, deterministic
failure, not a timing/flake issue)

### Root cause (unconfirmed hypothesis)

`RecordableServerHttpRequestTests.java:91` constructs an
`InetSocketAddress` (almost certainly via a literal/unresolved-host
constructor like `new InetSocketAddress(host, port)` or
`InetSocketAddress.createUnresolved`, given the test is exercising
`RecordableServerHttpRequest`'s address-formatting logic, not doing a real
network operation) and calls `.getAddress()` on it, expecting a non-null
`InetAddress`. Under CratonVM this returns `null` where HotSpot returns a
resolved/synthesized `InetAddress`. This points at a gap in CratonVM's
`InetSocketAddress`/`InetAddress` construction or resolution path (native
`java.net.InetSocketAddress.<init>`/`InetAddress.getByName`-family), not
investigated further at the source level this session (not checked against
`native-builtins/src/net*.rs`). The exact `InetSocketAddress` construction
site in the test would need reading
(`org.springframework.boot.webflux.actuate.web.exchanges.RecordableServerHttpRequestTests.java:91`
and its surrounding test setup) to know whether this is a resolved-host,
unresolved-host, or literal-IP construction — that distinction changes
which native function is implicated.

## Issue B — `WebFluxManagementChildContextConfigurationIntegrationTests`: HANG, unconfirmed

| Class | Note |
|---|---|
| `WebFluxManagementChildContextConfigurationIntegrationTests` | HANG |

`.out.log` is 0 bytes. `.err.log` (582 lines) shows normal JUnit-discovery
startup noise (`Post-clinit fixup` lines, the same benign `gc::guard`
out-of-bounds-field-read warning discussed in
[`spring-boot-configuration-processor-testcompiler-hang-cluster.md`](spring-boot-configuration-processor-testcompiler-hang-cluster.md)
and
[`spring-boot-restclient-residuals.md`](spring-boot-restclient-residuals.md))
cycling across what looks like 3 different nested test-class contexts
(3 distinct `InterceptingExecutableInvoker` object addresses over the
file), then the log simply stops at `21:11:49` — ~35s after the run
started — with no further output, no exception, no JUnit summary, and no
final gc::guard burst indicating another GC cycle occurred. This is a much
shorter apparent stall window than the other two hang clusters above (35s
vs. their multi-minute full-timeout runs), which may mean this class was
killed by a shorter-than-default timeout, or that it genuinely wedges very
early relative to its siblings.

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-webflux.org.springframework.boot.webflux.autoconfigure.actuate.web.WebFluxM-4f03cf739f2e.err.log`

### Root cause (unconfirmed hypothesis)

`WebFluxManagementChildContextConfigurationIntegrationTests` (per its name)
starts a reactive **child** `ApplicationContext` with its own embedded web
server (the actuator "management context on a separate port" pattern) —
this is exactly the kind of test shape that has repeatedly hit
already-documented CratonVM network/server-startup deadlocks in other
suites (GC-safepoint deadlocks during embedded-server boot, JSSE/TLS
handshake stalls, socket-bind races — see the `reference_*` net/HTTP/Tomcat
entries in project memory). No evidence beyond the log gap ties this to
any specific one of those families; this is a plausible category, not a
confirmed mechanism. Confirming needs a live repro with a stack dump on
timeout (not attempted this session, per its scope of log-only
investigation).

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-webflux` | `org.springframework.boot.webflux.actuate.web.exchanges.RecordableServerHttpRequestTests` |
| `module/spring-boot-webflux` | `org.springframework.boot.webflux.autoconfigure.actuate.web.WebFluxManagementChildContextConfigurationIntegrationTests` |
