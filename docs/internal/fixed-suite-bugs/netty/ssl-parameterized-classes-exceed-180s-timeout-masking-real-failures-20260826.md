# The four `SSLEngineTest` subclasses that reported HANG — the cap, and the two real defects it was hiding

**Status: RETIRED 2026-08-26.** Both of this page's open questions are answered,
and neither answer is the one it proposed.

* **Question 1 — "is 180s simply too short for this class family?"** Yes, and not
  marginally: **every one of the five classes exceeds the cap on HOTSPOT**, by
  1.1x to 5.3x, before CratonVM is involved at all. They are now in
  `apps/netty-suite-runner/class-overrides.tsv`.
* **Question 2 — "is `mySetupMutualAuth`'s `useTasks=true` failure a real
  CratonVM defect?"** It is a real defect, and `useTasks` has nothing to do with
  it. It fails for **every** parameterisation, it is not a TLS defect, and it is
  not even in the SSL layer: a WILDCARD TCP bind was AF_INET where HotSpot's is
  dual-stack, so the IPv6 loopback client `NetUtil.LOCALHOST` resolves to was
  refused with a RST. Un-capping the classes then exposed a **second** defect
  this page never saw, in `X509TrustManagerImpl`: **hostname verification was
  not performed at all** for callers of the `X509ExtendedTrustManager`
  overloads, so a certificate issued for a different host was accepted.

Both are fixed. Numbers below.

## Why one HANG hid all of it

`OpenSslEngineTest` and its three siblings run a protocol × cipher × delegate ×
useTasks × useTickets matrix — 821 to 4 088 test cases per class, each driving a
real in-JVM TLS handshake. The harness's flat 180s per-class wall cap kills the
fork and records `HANG`, which loses every result the class had already produced:
the PASSes, and the assertion failures the raw log had already printed before the
kill truncated it.

That is what made this look like one problem. It was three: a cap that no run of
these classes can fit inside, and two unrelated VM defects inside the part of the
matrix the cap was cutting off.

## The cap — measured on HotSpot, which settles it without CratonVM

One class per run for `OpenSslEngineTest`, the other four four-way concurrent,
HotSpot 25.0.3+9, same host, `common.args`:

| class | tests | HotSpot wall | vs the 180s cap |
| --- | ---: | ---: | ---: |
| `JdkSslEngineTest` | 821 | **192 s** | 1.07x |
| `OpenSslEngineTest` | 3 992 | **567 s** | 3.15x |
| `OpenSslJdkSslEngineInteroptTest` | 2 645 | **702 s** | 3.90x |
| `JdkOpenSslEngineInteroptTest` | 2 309 | **744 s** | 4.13x |
| `ReferenceCountedOpenSslEngineTest` | 4 088 | **952 s** | 5.29x |

**Zero failures in all five on HotSpot** (`aborted` is JUnit assumption
failures — 66/72/24/0/72 — which the runner already treats as skips).

So the `HANG` was never evidence about CratonVM. This page guessed these classes
"were very likely never a HANG on whatever host the 2026-08-13 baseline was
captured against"; the table says they cannot have passed under a 180s cap on any
host, and the baseline simply predates their inclusion.

## Defect 1 — a wildcard TCP bind was AF_INET, so every IPv6 loopback client was refused

### What the assertion actually says

```
org.opentest4j.AssertionFailedError: expected: <true> but was: <false>
	at io.netty.handler.ssl.SSLEngineTest.mySetupMutualAuth(SSLEngineTest.java:1302)
	at io.netty.handler.ssl.SSLEngineTest.testMutualAuthDiffCerts(SSLEngineTest.java:662)
```

This page read line 1302 as *"asserts the mutual-TLS handshake it just drove
actually completed"*. It is not. Line 1302 is

```java
ChannelFuture ccf = cb.connect(new InetSocketAddress(NetUtil.LOCALHOST, port));
assertTrue(ccf.awaitUninterruptibly().isSuccess());
```

— the client's **TCP connect**, before any TLS. `assertTrue` throws the future's
cause away, which is the whole reason this read as a TLS defect for two weeks.

### The measurement

`probes/SslMutualAuthConnectProbe.java` is `mySetupMutualAuth` reduced to that
one line, printing `ccf.cause()` instead of asserting. Same host, same argfile,
one run each:

```text
HotSpot   @@CONNECT case=diffCerts success=true  remote=/0:0:0:0:0:0:0:1:50312
CratonVM  @@CONNECT case=diffCerts success=false serverLocal=/0.0.0.0:58232
                    serverClass=Inet4Address remote=/0:0:0:0:0:0:0:1%{3C307829-…}:58524
CratonVM  @@CONNECT-CAUSE diffCerts io.netty.channel.AbstractChannel$AnnotatedConnectException:
                    finishConnect: Connection refused: /[0:0:0:0:0:0:0:1]:58524
```

`serverLocal=/0.0.0.0` is the answer. `sb.bind(new InetSocketAddress(0))` is a
wildcard bind; `sun.nio.ch.Net.serverSocket` opens **AF_INET6 with
`IPV6_V6ONLY` cleared** whenever IPv6 is available and the channel carries no
explicit `StandardProtocolFamily.INET`, so on HotSpot one listener accepts both
families. CratonVM bound a real AF_INET listener, and netty's `NetUtil.LOCALHOST`
is `::1` on a dual-stack Windows host — so the SYN got a RST.

`probes/WildcardBindFamilyProbe.java` turns that into a seven-row parity table
across every wildcard spelling and both client surfaces
(`SocketChannel` and `java.net.Socket`). **HotSpot 7/7; CratonVM 5 FAIL before,
0 after.** Two of the seven rows are NEGATIVE controls that a too-wide fix would
break — `ServerSocketChannel.open(StandardProtocolFamily.INET)` and an explicit
`bind(127.0.0.1)` must stay v4-only — and they pass in all three columns.

| row | HotSpot | CratonVM before | CratonVM after |
| --- | --- | --- | --- |
| `ssc.open()` | `[::]`, both families | `0.0.0.0`, v4 only | `[::]`, both |
| `ssc.open(INET6)` | `[::]`, both | `0.0.0.0`, v4 only | `[::]`, both |
| `ssc.open(INET)` *(negative control)* | `0.0.0.0`, v4 only | v4 only | v4 only |
| `ssc.bind(127.0.0.1)` *(negative control)* | v4 only | v4 only | v4 only |
| `ssc.bind(null)` | `[::]`, both | `0.0.0.0`, v4 only | `[::]`, both |
| `new ServerSocket(0)` | both | v4 only | both |
| `ServerSocket().bind(0)` | both | v4 only | both |

### The fix

`fd_table::open_tcp_dual_stack_listener` — the stream twin of
`open_udp_dual_stack_socket`, which had already been written for exactly this
defect on the datagram side (`DnsNameResolverTest.testTimeoutNotCached`). Three
call sites take it for a wildcard address, and only for a wildcard address:

* `socket_channel.rs::bind_wildcard_listener` — `ServerSocketChannel.bind`,
  which is netty's path. `decode_protocol_family` learns to record an explicit
  `INET`, because that is the one spelling that must NOT be widened;
* `net_phase_e.rs::re2_bind_with_pending_options` — the synthetic
  `java.net.ServerSocket`. **Both** of its arms, because the retained-option arm
  (`setReuseAddress(true)` before `bind`) builds its own socket and would
  otherwise have silently kept the v4-only listener;
* `net.rs::net_bind0` — the real-JDK-bytecode path, whose comment claimed *"we
  always bind dual-stack via Rust std"* and did not. Its `preferIPv6` argument
  is the JDK's own family decision for that fd, so it is exactly the bit that
  decides.

`ServerSocket.getInetAddress()` still answers `0.0.0.0` for a wildcard bind, as
on HotSpot, even though the socket underneath is now AF_INET6: `ServerSocket`
reports the address the caller asked for. The probe prints both halves so that
is checked rather than assumed.

Pinned by a unit test,
`fd_table::tests::dual_stack_wildcard_listener_accepts_both_loopback_families`,
which connects both loopback families to one wildcard listener and skips (rather
than fails) on a host with no IPv6 stack.

## Defect 2 — the extended `TrustManager` overloads did not identify the endpoint

Un-capping the classes made this visible for the first time: **48 of 48
parameterisations of `testClientHostnameValidationFail`, in three classes**,
reporting

```
unexpected exception: java.lang.IllegalStateException: handshake complete. expected failure
```

The test points a client at `localhost` and gives the server
`notlocalhost_server.pem`. The handshake must fail. It completed.

### Ruling things out, in order

`probes/OpenSslEndpointIdentProbe.java` supplies its own recording
`X509ExtendedTrustManager` and prints, from inside the callback, every input the
decision is made from. **Every one is identical on both VMs**:

```text
@@TM overload=3-arg-SSLEngine authType=UNKNOWN engine=io.netty.handler.ssl.OpenSslEngine
     endpointIdentificationAlgorithm=HTTPS peerHost=localhost
     handshakeSession=true sessionClass=…ReferenceCountedOpenSslEngine$2(extended)
     peerHostViaSession=localhost subject=CN=NOTlocalhost

HotSpot   @@TM delegate=REJECTED CertificateException: No name matching localhost found
CratonVM  @@TM delegate=ACCEPTED (no CertificateException)
```

So netty picked the right callback, the algorithm reached the engine, the session
was extended and carried the host, and the peer certificate was the wrong one.
The JDK's trust manager simply did not object.

`probes/HostnameCheckerProbe.java` clears the JDK's own checker of any part in
it: called directly on the same certificate, **CratonVM answers
`No name matching localhost found` too**. The checker works; it was never
reached.

### The cause

`x509_manager::register_trust_manager` registered all three
`checkServerTrusted` / `checkClientTrusted` descriptors onto the **two-argument**
handler, under a comment reading:

> The extra `Socket`/`SSLEngine` argument is advisory in JSSE (it exists so an
> implementation CAN consult the connection); the chain and authType are the
> whole input to the decision this module makes.

That is false for `sun.security.ssl.X509TrustManagerImpl`, and it is the exact
inversion of the truth: the three-argument overloads are the **only** place
endpoint identification runs. `checkTrusted` reads
`engine.getSSLParameters().getEndpointIdentificationAlgorithm()` off that
"advisory" argument and, when it is non-empty, calls `checkIdentity` →
`HostnameChecker.match`. The two-argument overload checks the chain and, by
design, no name at all. Collapsing them onto one handler removed hostname
verification from every caller of the extended API.

netty's OpenSSL provider is one such caller —
`ReferenceCountedOpenSslClientContext.ExtendedTrustManagerVerifyCallback` calls
`checkServerTrusted(chain, auth, engine)` from BoringSSL's verify callback — and
it has no other hostname check of its own.

### This VM already knew, in one place, and worked around it

`t27_tls::jsse_owns_endpoint_identification` carries a comment naming this exact
test:

> On THIS VM its `checkServerTrusted` is served by a native shim … it never sees
> the `SSLEngine`, so it cannot read
> `SSLParameters.getEndpointIdentificationAlgorithm()`. … Answering "the
> application owns identification" for a class whose identification code this VM
> does not run means NOBODY runs it: `testClientHostnameValidationFail` … it
> completed.

That predicate makes the VM's **own** rustls engine identify the endpoint
itself, which is why the pure-JDK provider arm passes 12/12 here. What it could
not cover is a TLS stack that is not this VM's — and netty-over-BoringSSL is
exactly that. The workaround was in the right place for its caller and in the
wrong place for the defect; the shim is now fixed at the source and that comment
is corrected rather than removed (the predicate stays `true`, because the two
checks agree when both run and this VM's engine need not carry a handshake
session).

### The fix

`check_server_trusted_extended` / `check_client_trusted_extended` run the chain
check exactly as before, then re-derive `X509TrustManagerImpl.checkIdentity` over
this module's own `verify_hostname` — the same routine the HTTPS client path
uses, so the VM's two identity checks cannot drift apart. The JDK's **order** is
reproduced because it is load-bearing: SNI host name first, fall back to the peer
host, and fail rather than fall back when the SNI name IS the peer host.

One deliberate divergence, commented at the site: a peer that reports no host at
all is **skipped** rather than refused with the JDK's
`CertificateException: No handshake session`. On HotSpot an engine reaching a
trust manager always has a session; in this VM the same overload is also called
by `t27_tls`'s rustls engine, whose `SSLEngine` object need not carry one — and
that path identifies separately. Throwing there would break every handshake that
stack makes in order to close a hole it does not have. It is logged, not silent.

Verified on both providers, on the same probe that measured the defect:

| arm | before | after |
| --- | --- | --- |
| netty OPENSSL provider | `@@VERDICT FAIL hostname-verification-not-enforced` | `@@VERDICT PASS` |
| netty JDK provider (control, was already passing) | `@@VERDICT PASS` | `@@VERDICT PASS` |

The JDK-provider arm is worth its line: its `peerHostViaSession` is **null**, so
it is the `SSLEngine.getPeerHost()` fallback above that carries it. A fix that
read only the session's host would have left that arm unidentified.

### The first version of this fix REGRESSED mutual TLS, and how that was caught

Worth recording, because the regression was subtle, deterministic, and nothing
about the fix's own subject predicted it.

The first cut read the algorithm with `engine.getSSLParameters()` — which is
what the JDK's own `X509TrustManagerImpl` does. On netty's
`ReferenceCountedOpenSslEngine` that method is `synchronized` and re-enters
tcnative (`SSL.getOptions`, and `SSL.getCiphers` via
`super.getSSLParameters()`), and these handlers run **inside BoringSSL's
certificate callback** whenever netty is configured `setUseTasks(false)`. The
client's TLSv1.3 `Certificate` flight was then never sent, and the server ended
the handshake with `PEER_DID_NOT_RETURN_A_CERTIFICATE`.

`probes/OpenSslTls13ClientCertProbe.java` — a real mutual-TLS handshake over
protocol x `useTasks` x transport, eight rows — caught it:

| binary | rows |
| --- | --- |
| HotSpot 25 | **8/8 PASS** |
| CratonVM, before this fix | **8/8 PASS** |
| CratonVM, first cut of this fix | **2 FAIL** — `TLSv1.3 x useTasks=false`, on `::1` AND `127.0.0.1` |

In every affected test the algorithm is explicitly `null`
(`SslContextBuilder…endpointIdentificationAlgorithm(null)`), so the entire cost
was one call that discovered there was nothing to do. The fix reads netty's
`endpointIdentificationAlgorithm` FIELD instead when the receiver is a
`ReferenceCountedOpenSslEngine` — no Java, no allocation, no native — and keeps
the method route for engines that have no such field. The underlying VM
sensitivity is NOT fixed, only avoided at this call site, and is written up in
`known-issues/netty/java-reentry-from-boringssl-verify-callback-loses-the-tls13-client-cert-20260826.md`.

**A note on the control that nearly hid this.** The first attempt to decide
"pre-existing or regression" ran the pre-fix binary with
`-Djava.net.preferIPv4Stack=true`, on the assumption that this would make netty
reach the server over IPv4 and so neutralise the wildcard-bind defect. It does
not: `SslMutualAuthConnectProbe` prints `isIpV4StackPreferred=true` and
`LOCALHOST=/0:0:0:0:0:0:0:1%{…}` in the same breath. The arm built on it
compared nothing, and its numbers (9 failures vs 15) were read as evidence in
both directions before the probe above settled it by binding each loopback
address explicitly. **A control has to be measured, not assumed.**

## What the five classes report now

## What the classes report now

Per-method, `OpenSslEngineTest`, on the fixed binary, `MethodRunner` (which
prints and times every case). **On a quiet host** — this box is shared, and the
section after this one explains why that qualifier is load-bearing:

| method | pre-fix | first cut of Defect 2's fix | fixed |
| --- | --- | --- | --- |
| `testMutualAuthDiffCerts` | **0 / 48** | 36 / 48 | **48 / 48** |
| `testClientHostnameValidationFail` | **0 / 48** | 48 / 48 | **48 / 48** |
| `testClientHostnameValidationSuccess` | 48 / 48 | — | **48 / 48** |
| `testMutualAuthSameCertChain` | 47 / 48 * | 28 / 48 | **48 / 48** |
| `mustCallResumeTrustedOnSessionResumption` | 36 / 48 | 36 / 48 | 36 / 48 |
| the other 13 methods | — | 48 / 48 | **48 / 48** |

\* `testMutualAuthSameCertChain` is the one method that does NOT dial
`NetUtil.LOCALHOST`; it connects to `serverChannel.localAddress()` verbatim, so
before the bind fix it reached the server over IPv4 and Defect 1 never bit it.
That is worth knowing, because it is also what made an early reading of the
residual blame the IPv6 transport.

`testClientHostnameValidationSuccess` is on that table as the POSITIVE control
for Defect 2's fix, and it is there because it nearly was not. A whole-class run
showed it taking **~28 minutes per invocation** and failing one — on a lightly
loaded box, with a 120 s JUnit cap configured that was plainly not firing. Run on
its own it is **48/48 in 44 s on the fixed binary and 48/48 in 48 s on the
pre-fix one**, against HotSpot's 16.6 s. So the stall is real but intermittent
and belongs to neither defect on this page; what it is NOT is this fix rejecting
a certificate it should accept, which is the thing a positive control exists to
rule out. `probes/OpenSslTls13ClientCertProbe.java`'s two `identify=HTTPS` rows
check the same property in two seconds and pass.

`mustCallResumeTrustedOnSessionResumption` is unchanged by any of this and is
the one real failure left in the class: **the same twelve invocations fail on
the pre-fix and post-fix binaries**, HotSpot passes 48/48, and it installs its
own trust manager so it never reaches the code this page changed. It has its own
page: `known-issues/netty/java-reentry-from-boringssl-verify-callback-loses-the-tls13-client-cert-20260826.md`.

**Scope of the inventory, stated rather than implied.** 707 of the class's
3 992 cases were run to completion on a quiet host — 17 of its 19 methods, whole
— covering every method this page's two defects touched. A full 3 992-case pass
is scheduled to run when the machine is next idle; it is not folded in here
because the attempt that ran while a concurrent build had the box at 0 GB free
produced 58 "failures" of which the diagnostic one was

```
java.lang.InternalError: JIT dispatch into java/lang/Thread.start()V failed:
  failed to spawn child Java thread (OS refused ...) Os { code: 1450 }
```

— the OS refusing to create threads. **A saturated host does not produce a
weaker measurement, it produces a different one**, and counting those 58 as
results would have put two fixed methods back on this page as broken.

## The override table

`apps/netty-suite-runner/class-overrides.tsv` now carries all five, with the
HotSpot walls above written into the file as the justification:

| class | floor |
| --- | ---: |
| `io.netty.handler.ssl.JdkSslEngineTest` | 3 600 s |
| `io.netty.handler.ssl.OpenSslEngineTest` | 14 400 s |
| `io.netty.handler.ssl.OpenSslJdkSslEngineInteroptTest` | 10 800 s |
| `io.netty.handler.ssl.JdkOpenSslEngineInteroptTest` | 10 800 s |
| `io.netty.handler.ssl.ReferenceCountedOpenSslEngineTest` | 14 400 s |

`run-netty-suite.sh overrides` prints `state: 6 (loaded)`.

The rate behind them is measured on the fixed binary: **1.19 s per case** over
the 672 cases that are not `mustCallResumeTrustedOnSessionResumption`, against
HotSpot's ~0.14 s — about **8.4x**, not the ~14x an earlier revision of this page
and of the table's own comment claimed. That figure came off the binary carrying
the regression described above, whose extra failures inflated the mean: **a
per-case rate measured on a binary with a known defect in the workload is a rate
for the defect.**

One method is most of the wall: 2 618 s across 31 invocations against 798 s for
the other 672 cases together, because each of its twelve failures burns a 60 s
JUnit timeout. Closing that page would take this class from roughly 2.4 h to
1.5 h and would be the way to lower these floors — not editing the table.

**The floors are expensive and that is a real trade.** A four-hour cap on a
class changes a suite whose whole 657-class run was 178 minutes. What it buys is
the per-class `ok`/`failed` breakdown that a `HANG` destroys: the two defects on
this page sat inside one for two weeks, and one of them was accepting a
certificate issued for the wrong host.

## Repro

```bash
cd apps/netty-suite-runner
# the four probes, HotSpot first then CratonVM, same argfile
javac @cp-javac.args -d /tmp/cls ../../probes/WildcardBindFamilyProbe.java \
    ../../probes/SslMutualAuthConnectProbe.java ../../probes/OpenSslEndpointIdentProbe.java
java  -cp /tmp/cls WildcardBindFamilyProbe                    # 7 rows, exit 1 on any FAIL
cratonvm --java-home <jdk25> --Xmx 512m -cp /tmp/cls WildcardBindFamilyProbe

# one whole class, un-capped
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout 7200 \
  cratonvm --java-home <jdk25> --Xmx 1500m -XX:+UseG1GC \
  @common.args -Dcraton.batch=1 CratonRunner io.netty.handler.ssl.OpenSslEngineTest

# one method across its whole parameter matrix (14 s on HotSpot, the fast A/B)
java @common.args MethodRunner \
  'io.netty.handler.ssl.OpenSslEngineTest#testClientHostnameValidationFail(io.netty.handler.ssl.SSLEngineTest$SSLEngineTestParam)'
```

## Related files

- `native-api/src/fd_table.rs` — `open_tcp_dual_stack_listener`
- `native-io/src/socket_channel.rs` — `bind_wildcard_listener`, `decode_protocol_family`
- `native-io/src/net.rs` — `net_bind0`
- `native-builtins/src/net_phase_e.rs` — `re2_bind_with_pending_options`
- `native-builtins/src/x509_manager.rs` — `check_server_trusted_extended`
- `native-builtins/src/t27_tls.rs` — `jsse_owns_endpoint_identification`
- `probes/WildcardBindFamilyProbe.java`, `probes/SslMutualAuthConnectProbe.java`,
  `probes/OpenSslEndpointIdentProbe.java`, `probes/HostnameCheckerProbe.java`
- `apps/netty-suite-runner/class-overrides.tsv`
