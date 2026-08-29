# `JettyReactiveWebServerFactoryTests.sslNeedsClientAuthenticationFailsWithoutClientCertificate` — the client never observes the rejected connection closing

## Status
**OPEN, intermittent — 1 failure in 219 CratonVM runs.** Observed once, in the
2026-08-29 full Spring Boot suite run on the Azure Linux host. Not reproduced
since, on either platform, under any collector, at any load. The failing run's
timestamps identify the mechanism, and they point the **opposite way** from
the first triage.

This page replaces the Jetty half of
`jettyreactive-tls-timeout-and-webmvcendpoint-autoconfig-20260829.md`. That
page's other half — `WebMvcEndpointIntegrationTests` — was a different defect,
root-caused and fixed in `f62216ca0`
(`internal/fixed-suite-bugs/springboot/webmvcendpoint-annotation-proxy-cache-outlived-its-class-FIXED-20260829.md`).
Nothing is shared between the two.

## Symptom

```
java.lang.AssertionError: VerifySubscriber timed out on
  reactor.core.publisher.MonoFlatMap$FlatMapMain@7234
  at ...AbstractReactiveWebServerFactoryTests.testClientAuthFailure(...:344)
```

The test starts a Jetty reactive server with `Ssl.ClientAuth.NEED`, connects a
`WebClient` that presents no client certificate, and asserts the reactive
chain terminates with a `WebClientRequestException`:

```java
StepVerifier.create(result)
    .expectError(WebClientRequestException.class)
    .verify(Duration.ofSeconds(10));
```

## What the timestamps say

From the failing run's own `.out.log`:

```
03:20:46.661  Jetty started on port 41823 (ssl, http/1.1)
03:20:57.296  Stopped SslValidatingServerConnector ...{0.0.0.0:0}
03:21:07.725  [reactor-http-nio-4] WARN HttpClientConnect -- The connection observed an error
              io.netty.channel.StacklessClosedChannelException
                Suppressed: StacklessSSLHandshakeException:
                  Connection closed while SSL/TLS handshake was in progress
```

* server up at `46.661`;
* `57.296` — **10.6 s later** — the connector is stopped. That is the test's
  own cleanup running after `verify(Duration.ofSeconds(10))` expired;
* `07.725` — **another 10.4 s after the server was stopped** — the client
  finally sees the channel go inactive.

So the client did not miss a signal it was sent. **No signal arrived** while
the server was up. The handshake rejection never reached the wire as a close,
and the client only noticed once the connector had been torn down (and even
then on what looks like a second ~10 s timer, not on a prompt FIN).

The original triage read the suppressed
`StacklessSSLHandshakeException` as proof the server-side rejection worked and
concluded the fault was "client-side `WebClient`/Reactor Netty not propagating
the connection-reset". The ordering says otherwise: that exception is the
client's own record of the channel dying at `03:21:07`, 21 seconds after the
request began — it is the *consequence* of the late close, not evidence of a
timely one. **Attribute this to the server side not closing the rejected
connection**, until a packet capture says otherwise.

## Non-reproduction

Every arm below is CratonVM on the class or the method; all zero failures.

| host | arm | runs |
|---|---|---|
| Windows | full class, default / G1 / Generational collectors | 24 |
| Windows | full class, quiet | 16 |
| Windows | full class, 6 concurrent × 10 CPU spinners | 30 |
| Windows | the method alone, quiet | 60 |
| Windows | the method alone, 4 concurrent × 10 CPU spinners | 60 |
| Windows | full class, `--nojit` | 3 |
| Linux (the failing host) | full class, the binary that produced the failure | 16 |
| Linux (the failing host) | the method alone | 25 |
| | **total** | **219** |

The Linux arm matters most: same host, same fixture, and the *same binary
file* (`/data/cvm-h2serial-20260813/target/release/cratonvm`, mtime unchanged
since before the suite run) that produced the failure.

There is also no latency tail to point at. 25 method runs per VM on the Linux
host, whole-process wall clock:

| | min | median | max |
|---|---|---|---|
| CratonVM | 3468 ms | 4085 ms | 4652 ms |
| HotSpot 25 | 4334 ms | 4842 ms | 5603 ms |

CratonVM is *faster* than HotSpot here, and neither VM produced a single run
anywhere near the 10 s verify budget. Whatever happened at 03:20 is not a
close path that is merely slow; it is one that, rarely, does not fire at all.

## Next step

The decisive experiment is at the packet level, not in Reactor Netty: run the
test under `tcpdump`/`strace` on the Linux host in a loop until it trips, and
answer one question — **does the server ever send a FIN or RST when it rejects
the handshake?**

* If it does not, the defect is in CratonVM's socket-close path on the
  server side (Jetty's `SSLEngine` reject → `EndPoint.close()` → the VM's
  `java.net.Socket`/`SocketChannel` close), and Reactor Netty is behaving
  correctly by waiting.
* If it does, the search moves to the client and the original page's framing
  was right after all.

Given a 1-in-219 rate, budget for a long loop; the suite run that found it was
1991 classes deep.

## Repro (Linux, the host of record)

```bash
/data/mtls-lat.sh <cratonvm-bin> <N> <tag>
# module root: /data/cratonvm/apps/spring-boot/module/spring-boot-jetty
# env the runner sets and that is load-bearing:
#   CRATONVM_REAL=net-sockets,aqs CRATONVM_THREADS=-default-watchdog
#   CRATONVM_JIT=rootsnap-cache
```

Windows equivalent: `apps/spring-boot-suite-runner/run-single-class.ps1
-Module module/spring-boot-jetty -ClassName
org.springframework.boot.jetty.reactive.JettyReactiveWebServerFactoryTests`.

## Note on the run that found it

That suite run printed in its own header:

```
hotspot baseline: none -- every failure will be attributed to CratonVM
```

The HotSpot arm has since been run for this class on the Linux host: 37 pass,
1 skip, identical to CratonVM's own passing runs. So there is no evidence this
is an upstream flake — but there is also no HotSpot failure to compare the one
CratonVM failure against, and 25 HotSpot method runs produced none.
