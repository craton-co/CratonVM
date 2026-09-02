# `JettyReactiveWebServerFactoryTests.sslNeedsClientAuthenticationFailsWithoutClientCertificate` — reproduced, and HotSpot fails it more often

## Status
**RETIRED, 2026-09-01. Reproduced, packet-captured, and paired against HotSpot.
It is not a CratonVM defect: on the same host, in the same bursts, in the same
window, HotSpot fails it at 5.0 % and CratonVM at 2.8 %.**

The page asked one decisive question:

> **does the server ever send a FIN or RST when it rejects the handshake?**

The question contains a false premise. **There was no handshake to reject.** On
the failing connection neither side ever wrote a byte: no `ClientHello`, no
`ServerHello`, no TLS at all. The TCP connection was completed by the KERNEL —
`SYN`/`SYN-ACK` happen in the accept queue with no application involvement —
and then sat unserviced for 14.3 seconds while the test's 10-second budget
expired.

Neither attribution on the record was right:

* not "the client is not propagating the connection-reset" (the first triage) —
  there was no reset to propagate, and the client had not even sent its
  `ClientHello`;
* not "the server side is not closing the rejected connection" (this page) —
  the server closed promptly once it was told to, and its `FIN` at +14.3 s IS
  the test's own cleanup stopping the connector.

Both JVM threads simply failed to be scheduled, on a host at load 64.59 on
8 cores.

## The packet capture

`tcpdump -i any -s 96 -U` over the whole loop, filtered to `SYN|FIN|RST`, with
each run's wall-clock window recorded beside its result. Relative sequence
numbers, so `seq N` on a `FIN` means `N-1` application bytes were sent before
it and `ack N` means `N-1` were received.

**A PASSING connection** (port 38113, and a second identical one in another
run):

```
1788297908.436805  S    127.0.0.1.47120 > 127.0.0.1.38113
1788297908.436829  S.   127.0.0.1.38113 > 127.0.0.1.47120
1788297909.202684  F.   127.0.0.1.38113 > 127.0.0.1.47120   seq 1932, ack 482
1788297909.210140  F.   127.0.0.1.47120 > 127.0.0.1.38113   seq 482,  ack 1933
```

The server wrote **1931 bytes** (`ServerHello`, `Certificate`,
`CertificateRequest`, `ServerHelloDone`), the client wrote **481** (its
`ClientHello`), and the server FIN'd **766 ms** after the SYN. That is the
rejection working, and it is prompt.

**The FAILING connection**, from the run that tripped:

```
1788302412.428503  S    127.0.0.1.37842 > 127.0.0.1.38113
1788302412.428530  S.   127.0.0.1.38113 > 127.0.0.1.37842
1788302426.773950  F.   127.0.0.1.38113 > 127.0.0.1.37842   seq 1, ack 1
1788302427.631200  R    127.0.0.1.38113 > 127.0.0.1.37842
1788302427.962424  R.   127.0.0.1.38113 > 127.0.0.1.39126   (a later connect, listener gone)
```

**`seq 1` and `ack 1`.** Zero bytes out of the server, zero bytes in from the
client, for **14.345 seconds**. The `ack` is the load-bearing half and it is
not a capture artefact: this is loopback, so had the client written its
`ClientHello` the server's TCP would have acked it immediately whatever the
application was doing. It acked nothing, so nothing was sent.

The matching test log:

```
22:40:09.989 [main]  Jetty started on port 38113 (ssl, http/1.1)
22:40:27.898 [reactor-http-nio-2] WARN HttpClientConnect -- The connection observed an error
      Suppressed: StacklessSSLHandshakeException:
        Connection closed while SSL/TLS handshake was in progress
=> java.lang.AssertionError: VerifySubscriber timed out on
   reactor.core.publisher.MonoFlatMap$FlatMapMain@2942
```

— the same assertion, the same suppressed exception, and the same ~18-21 s
shape as the original 2026-08-29 failure. Reactor Netty is behaving correctly
throughout: it is waiting for a handshake that was never started.

## How it was reproduced

Not by any arm the page's non-reproduction table contains, and not by extending
them. Three CratonVM workers launched **at the same instant** while a
regression suite was compiling: **all three tripped on their first iteration**,
at `load=64.59`.

That is the shape of the original, which the page records as found "1991
classes deep" in a full suite — many JVMs, cold, at once. What a steady loop
loses is the SIMULTANEITY: workers drift apart within a minute, and after that
each JVM's class loading overlaps the others' steady state instead of their
startup.

Starving the VM is NOT sufficient on its own. Pinned to two cores the whole
test takes up to **36 seconds** of wall clock and still passes, because the
10-second budget is on the `StepVerifier`, not on the process. It takes a burst
of simultaneous cold JVM starts.

## The HotSpot arm — which is the whole answer

The page's table has 219 CratonVM runs and **no HotSpot column at all**, and it
says so: "there is also no HotSpot failure to compare the one CratonVM failure
against". A CratonVM-only loop cannot tell "this VM is wrong" from "a
10-second wall-clock budget does not survive an oversubscribed host", and that
is the whole question. Every arm below runs **both VMs**, interleaved or in the
same burst, so neither is measured against a load the other did not see.

| arm | load | CratonVM | HotSpot |
|---|---|---:|---:|
| steady loop, 3 workers (before the HotSpot arm existed) | 20-35 | 1161 pass, 0 fail | — |
| steady loop, 3+3 workers | 16-32 | 33 pass, 0 fail | 52 pass, 0 fail |
| pinned to 2 cores, 2+2 workers (up to 36 s wall per run) | 16-32 | 20 pass, 0 fail | 29 pass, 0 fail |
| synchronised bursts, K=3 (6 JVMs at once) | 27-30 | 30 pass, 0 fail | 30 pass, 0 fail |
| **synchronised bursts, K=6 (12 JVMs at once)** | **36-124** | **175 pass, 5 fail (2.8 %)** | **171 pass, 9 fail (5.0 %)** |
| the original trip | 64.59 | 3 of 3 failed | not running |

Every one of the 14 failures is the same `VerifySubscriber timed out`
assertion. **HotSpot fails it at 1.8x CratonVM's rate.** Whatever this is, it
is not something CratonVM does and HotSpot does not.

The page's own latency table already pointed this way and was not followed: it
recorded CratonVM at a median 4085 ms against HotSpot's 4842 ms for this
method, i.e. CratonVM *faster*, with neither VM anywhere near the 10 s budget.
A test that is comfortably inside its budget on both VMs and then fails on one
of them is a test whose budget was consumed by something neither VM controls.

## What is actually exposed

`AbstractReactiveWebServerFactoryTests.testClientAuthFailure` asserts with
`verify(Duration.ofSeconds(10))` — a **wall-clock** budget, in a test whose
work is two JVM-internal event loops handshaking over loopback. On a host where
a runnable thread gets a single-digit percentage of a core, that budget is not
met by anything, and the failure it produces is indistinguishable at the log
level from a real close-path defect. That is what put two successive triages on
the wrong subsystem for four days.

This is a species the regression suite already handles and the Spring Boot
runner does not: the regression suite prints its own `rc=124 is the harness's
own timeout, not a VM failure … re-run with TIMEOUT=600` for `RMapGcStress`.
There is no equivalent for a wall-clock assertion INSIDE a test, where the
harness never sees a timeout at all — it sees a clean assertion failure.

**Recommendation, not landed here:** record `/proc/loadavg` beside every Spring
Boot suite row, and re-run any class that produces a `VerifySubscriber timed
out` at load >> core count before attributing it. The suite run that produced
the original also printed `hotspot baseline: none -- every failure will be
attributed to CratonVM`, which is exactly the condition under which a
load-gated failure becomes a VM bug on paper.

## What this page got right

The timestamp analysis. It read the 10.6 s and 10.4 s gaps correctly, drew the
correct local conclusion — "no signal arrived while the server was up" — and
correctly rejected the first triage's reading of the suppressed
`StacklessSSLHandshakeException` as evidence of a timely close. It then
attributed the silence to the server's close path, which is one step further
than the timestamps could carry, and named the experiment that would settle it.
That experiment was the right one; it answered a question the page had not
asked.

## Repro

```bash
/data/mtls-burst.sh <cratonvm-bin> <rounds> <K> <outdir>
#   launches K CratonVM and K HotSpot runs of the method simultaneously,
#   waits for all 2K, repeats. Both arms in every burst.
# capture, in parallel, packet-buffered so a kill cannot lose it:
sudo tcpdump -i any -s 96 -U -Z root -w /tmp/cap 'tcp[tcpflags] & (tcp-syn|tcp-fin|tcp-rst) != 0'
```

Read the `FIN`'s `seq` and `ack`. `seq 1932, ack 482` is a working rejection;
`seq 1, ack 1` is a connection nobody serviced.

Module root `apps/spring-boot/module/spring-boot-jetty`; the load-bearing env
is `CRATONVM_REAL=net-sockets,aqs CRATONVM_THREADS=-default-watchdog
CRATONVM_JIT=rootsnap-cache`.

Three notes on the harness, each of which cost a wrong reading first:

* `-w` without `-U` BUFFERS, and killing `sudo` does not flush `tcpdump`. The
  first per-run capture wrote 24-byte header-only files for every run. One
  persistent packet-buffered capture, read after the fact, is the shape that
  works.
* `tcpdump`'s apparmor profile refuses to write under `/data`. It drops
  privileges too, so `-Z root` or a writable directory is needed.
* run against a PRIVATE copy of the binary. Another session deleted
  `/data/cratonvm/target/release/cratonvm` to rebuild it at iteration 392 of
  the first loop, producing a `rc=127` row that looked like a repro until the
  log said `timeout: failed to run command`. Screen on the runner's own
  `SBRUNNER_RESULT` line, not on the exit code.

`/data/mtls-lat.sh`, which the original Repro block names, no longer exists on
the host, and the binary it names
(`/data/cvm-h2serial-20260813/target/release/cratonvm`) has been rebuilt since
the page was written. Neither is recoverable.

## Related

- `jettyreactive-tls-timeout-and-webmvcendpoint-autoconfig-20260829.md` — the
  page this one replaced; its other half was a real defect, fixed in
  `f62216ca0`.
- `fixed-suite-bugs/springboot/webmvcendpoint-annotation-proxy-cache-outlived-its-class-FIXED-20260829.md`
