# `WebTestClient`/`WebClient` calling back into the test's own just-started embedded server hits a real OS-level connect timeout (`os error 10060`)

**Status: OPEN — found 2026-07-17**, while re-verifying
[`contextrunner-resource-cycle-then-silent-stall-cluster-FIXED.md`](../../internal/springboot/contextrunner-resource-cycle-then-silent-stall-cluster-FIXED.md)
with generous timeouts.

## Symptom

3 classes in `module/spring-boot-security`, all `ApplicationContextRunner`-style
tests that start a real embedded reactive/servlet web server (Tomcat) and then
issue a real HTTP request back to it via `WebTestClient`/`WebClient`, fail —
not hang — with:

```
=> org.springframework.web.reactive.function.client.WebClientRequestException: HttpClient request failed: [connection attempt failed because the connected party did not properly respond after a period of time, or the established connection failed because the connected host failed to respond] (os error 10060)
```

(`os error 10060` = Windows `WSAETIMEDOUT` — a real OS-level TCP connect
timeout, not a Java-level exception synthesized by CratonVM.)

| Module | Class | tests failed/total |
|---|---|---:|
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.reactive.EndpointRequestIntegrationTests` | 2/4 |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.servlet.JerseyEndpointRequestIntegrationTests` | 5/9 |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.servlet.MvcEndpointRequestIntegrationTests` | 5/9 |

Every failure is in a test method that starts a fresh embedded web server via
`ApplicationContextRunner`/`ReactiveWebApplicationContextRunner`, then
immediately issues a `WebTestClient` request against `localhost:<ephemeral
port>` (see e.g. `EndpointRequestIntegrationTests.toEndpointPostShouldMatch`,
`apps/spring-boot/module/spring-boot-security/src/test/java/org/springframework/boot/security/autoconfigure/actuate/web/reactive/EndpointRequestIntegrationTests.java:73-85`).

## How this was found

Discovered while re-verifying
[`contextrunner-resource-cycle-then-silent-stall-cluster-FIXED.md`](../../internal/springboot/contextrunner-resource-cycle-then-silent-stall-cluster-FIXED.md) —
that doc's 5 "hangs" turned out to just need more wall time than the 300s
shard default (confirmed by running each standalone with a multi-minute
timeout and a live per-OS-thread CPU sample showing continuous, genuine work,
never a parked thread). Once given enough time, these 3 classes reach a real
JUnit result instead of timing out, and the actual failure is this connect
timeout.

## Root cause — NOT independently confirmed this session

Not root-caused at the source level. Two candidate explanations, not
distinguished:

1. **Genuine host/network-stack issue on this shared Windows dev box** — an
   ephemeral-port exhaustion, firewall interaction, or general host
   contention (this box has repeatedly shown itself to be heavily
   oversubscribed by concurrent sessions — see
   `feedback_shared_host_multitenant_confound` in project memory) causing a
   loopback TCP connect to occasionally not complete its handshake within
   the client's timeout window. Would NOT be a CratonVM bug at all.
2. **A genuine CratonVM startup-readiness race**: the embedded Tomcat
   `WebServer.start()` call returns (and Spring logs "Started" /
   `ApplicationContextRunner.run()`'s lambda begins executing) before the
   listener socket's `accept()` loop is actually ready to service inbound
   connections on some other thread — a client connecting in that narrow
   window would see exactly a connect timeout (not "connection refused",
   which is what a definitely-not-listening port produces) if CratonVM's
   native socket/accept-thread bring-up has extra latency HotSpot doesn't.

`os error 10060` specifically (a *timeout*, not `os error 10061` /
`ECONNREFUSED`) is more consistent with hypothesis 2 — a refused connection
is near-instant on a genuinely closed port, while a timeout suggests the SYN
was sent but nothing answered inside the client's window, which typically
means either severe host scheduling delay or a listener that is bound but
not yet calling `accept()`.

## What would confirm this

- Compare against a same-shape real-HotSpot baseline run of these 3 classes
  on this same host at the same time (rules host contention in/out).
- Time-correlate the "Started ProtocolHandler"/"Starting service" log line
  against the `WebTestClient` request's first byte sent, on a build with
  extra tracing on the native accept-thread bring-up.
- Retry the same 3 classes on an idle host.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.reactive.EndpointRequestIntegrationTests` |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.servlet.JerseyEndpointRequestIntegrationTests` |
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.servlet.MvcEndpointRequestIntegrationTests` |
