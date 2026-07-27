# TLS handshake enforcement/validation gap — 8 classes — FIXED

**Status:** FIXED (2026-07-27), with two precisely-characterised residuals
listed at the bottom. Measured on the local Windows harness (`apps/tomcat`,
`apps/tomcat-suite-runner`), real JDK 25 boot, real sockets.

The doc this replaces framed the failures as "CratonVM enforces a rejection
condition too loosely, or throws the wrong exception type", and guessed at a
single shared root cause in "the real-socket JSSE emulation layer's
`SSLEngine`/`SSLContext`/`X509TrustManager` wiring". That framing was
directionally right about the AREA and wrong about nearly every specific: the
8 classes turned out to be **nine distinct defects**, one of which had nothing
to do with validation at all, and one of which is a pure throughput problem
that belongs to a different doc.

Several of them were stacked — each hid the next, which is why this needed ten
build/measure rounds rather than one. Notably, protocol enforcement (#2) could
not even be tested until the client-restriction mechanism was un-deadened
(#1b), and the moment it worked it exposed that **no TLS 1.2 server handshake
had ever completed through this SSLEngine** (#4).

## Result

| Class | Baseline | After |
|---|---|---|
| `org.apache.tomcat.util.net.TestSSLHostConfigProtocol` | 2 / 12 fail | **OK 12/12** |
| `org.apache.tomcat.util.net.TestSSLHostConfigCipher` | 2 / 12 fail | **OK 12/12** |
| `org.apache.tomcat.util.net.TestSslHandshakeFailure` | 1 / 1 fail | **OK 1/1** |
| `org.apache.tomcat.util.net.TestCustomSslTrustManager` | 2 / 9 fail | **OK 9/9** |
| `org.apache.catalina.valves.rewrite.TestResolverSSL` | 1 / 3 fail | **OK 3/3** |
| `org.apache.tomcat.util.net.TestSsl` (`testSni`) | fail | **passes** |
| `org.apache.tomcat.util.net.TestClientCert` | 5 / 18 fail | 1 / 18 (residual A) |
| `org.apache.tomcat.util.net.TestSSLHostConfigCompat` | 4 / 78 fail | 1 / 78 (residual B) |

`TestSsl` as a whole is no longer a TLS failure but remains slow — see §8.

## Root causes

### 1. The client-restriction mechanism was dead code

`HttpsURLConnection.setDefaultSSLSocketFactory` is registered as a native, and
it captured the factory's `SSLContext` but never wrote the factory into the
real JDK static field `HttpsURLConnection.defaultSSLSocketFactory`.
`http_url_connection`'s `huc_upcall_create_socket_if_custom_factory` — the only
code path that makes a caller-installed factory's cipher/protocol restriction
reach the handshake — reads exactly that field, so it always found `null` and
did nothing. Every "restrict the client, expect the handshake to fail" test
therefore connected completely unrestricted.

**Fix (first attempt — DID NOT WORK):** `publish_default_ssl_socket_factory`
(t27_tls.rs) wrote that static field. Every subsequent round still behaved as
if the mechanism were dead, and instrumenting the reader proved it was:
`CRATONVM_DBG_TLS_AUTH` reports `default factory is null` immediately after
the write, in both a passing and a failing test. **`ctx.set_static_field` on
`javax/net/ssl/HttpsURLConnection.defaultSSLSocketFactory` does not stick on
this VM.** Worth knowing before anyone else routes state through a JDK static.

**Fix (what actually works):** keep the factory in a GC-rooted native slot,
`t27_tls::huc_default_factory_slot`, scanned and remapped by
`gc_scan_tls_ctx_trust_manager_roots` / `gc_update_tls_ctx_trust_manager_refs`
alongside `ctx_trust_managers_table` (this module's only other
`ObjectRef`-holding table). The field write is still attempted so ordinary
reflective readers see it if the VM ever honours it, but nothing depends on it.

### 1b. …and on the layered path, both restrictions were dropped

The real JDK `HttpsURLConnection` does not reach TLS through the native
`perform` at all: it goes through the LAYERED
`SSLSocketFactory.createSocket(Socket, host, port, autoClose)` overload, whose
handshake is deferred. `phases_late/ssl_security.rs` handled that deferred case
for `setEnabledCipherSuites` — but `net_phase_e.rs` registers the same
`(class, method, descriptor)` triple LATER, and registry semantics are
last-writer-wins, so its version silently replaced the only handler that knew
about pending layered sockets. `setEnabledProtocols` had no handler at all.

**Fix:** both setters in `net_phase_e.rs` now route a pending layered socket to
`set_pending_layered_socket_{ciphers,protocols}`. This is the second
last-writer-wins collision in this area (cf. `reference_dual_registration_*`);
when adding a registration for a triple another module already covers, check
what behaviour that module's version had.

### 2. Nothing enforced TLS protocol versions

Every client and server config was built with
`with_safe_default_protocol_versions()` (TLS 1.3 + TLS 1.2), and
`SSLSocket.setEnabledProtocols` was registered as an unconditional no-op. A
connector or client pinned to one version happily spoke the other.

**Fix:** `protocol_versions_for` maps Java protocol names to rustls versions;
`server_builder_with_versions` and the client builders thread them through;
`SSLSocket.setEnabledProtocols` records the restriction (net_phase_e.rs, which
registers after phases_late and therefore wins).

### 3. Cipher suites and protocol versions are not independent

Enforcing (2) immediately exposed this. A client narrowed to TLS 1.2 suites
still advertised `supported_versions = [1.3, 1.2]`; the peer preferred TLS 1.3,
found no suite in common, and the handshake died — turning a restriction the
caller expected to SUCCEED into a `handshake_failure`. The mirror image on the
server: `with_protocol_versions` hard-errors ("no usable cipher suites
configured") when the restricted provider has no suite for a requested version,
and an error there aborts `engine_begin` before a single TLS byte is written,
so the peer just sees the connection close.

**Fix:** `provider_and_versions` resolves the pair together — the cipher list
may only narrow *within* an explicit protocol restriction, and an unenforceable
cipher restriction is dropped rather than a configured protocol version.

### 4. No TLS 1.2 server handshake had ever completed through the SSLEngine

`handshake_status_of` reported `FINISHED` the moment `is_handshaking()` went
false, without checking whether rustls still had records queued. A **TLS 1.2
server** queues its ChangeCipherSpec + Finished only AFTER processing the
client's Finished, so Tomcat's `SecureNioChannel.handshake` saw `FINISHED`,
stopped driving the handshake, and never wrapped the server's final flight onto
the wire. The peer waited for a Finished that never came.

TLS 1.3 hid this completely (the server's whole flight is sent before the
client's Finished arrives), and until (2) started working nothing in the suite
ever actually pinned a connector to TLS 1.2 — so the defect was latent behind
another defect.

**Fix:** demand one more `wrap` while `wants_write()` is true and the handshake
has not yet been reported finished.

### 5. `TLS_DHE_RSA_*` restrictions silently became "no restriction"

rustls implements no finite-field DHE in any provider, so those names mapped to
nothing and `cipher_provider_for` fell back to the *unrestricted* provider — a
deliberately incompatible restriction quietly negotiated something else and
succeeded.

**Fix:** map the `DHE_RSA` suites onto their ECDHE analogue, which differs only
in the key-exchange group and preserves the authentication algorithm and bulk
cipher — exactly the policy these tests discriminate on. Applied consistently
on both ends, and advertised from `SUPPORTED_CIPHER_SUITE_NAMES` so Tomcat's
`SSLUtilBase.getEnabled` stops dropping them from a connector's configured
list. `DHE_DSS` is deliberately NOT mapped: substituting an RSA-authenticated
suite would change the property being tested.

### 6. A rejected handshake reached Java as `IOException`

A TLS 1.3 client finishes its own side before the server has accepted it, so a
server rejection (`CertificateRequired` from a
`certificateVerification="required"` connector) arrives as a fatal alert while
reading the response, or as a failing first write. Only the exact
"connection closed before response head" string was reclassified.

**Fix:** the first request write/flush and any `received fatal alert` before a
response byte are now tagged with `TLS_HANDSHAKE_FAILURE_SENTINEL`, so Java
sees `SSLHandshakeException` as real JSSE does.

### 7. Client certificates: Tomcat renegotiates, rustls cannot

Tomcat's default `certificateVerification` is "none": it discovers a request
needs `CLIENT-CERT` auth only after parsing the request line, and collects the
certificate by RENEGOTIATING —
`NioEndpoint$NioSocketWrapper.doClientAuth` calls
`SSLEngine.setNeedClientAuth(true)` then `SecureNioChannel.rehandshake()`, i.e.
a second `beginHandshake()` on an engine whose handshake already finished.
rustls categorically does not implement renegotiation, so that call did nothing
and the connection was dropped with no response.

**Fix — deferred client auth.** The rehandshake attempt is detected in
`engine_begin`'s short-circuit branch; the connector is marked (keyed by the
owning `SSLContext`, so it is scoped to one connector and resets per test) and
the call fails fast so the connection closes immediately instead of stalling.
`http_url_connection::perform_with_retry` then transparently retries once on a
fresh connection, and THAT handshake carries an optional `CertificateRequest`,
so the client's real `KeyManager.chooseClientAlias` runs and the certificate is
on the session in time for `SSLAuthenticator` to find it — no renegotiation
needed.

The scoping matters: `TestClientCert` asserts
`getLastClientAuthRequestedIssuerCount() == 0` after the FIRST, unprotected
request of every test, which a process-wide or certificate-keyed marker would
break. The acceptable-CA list advertised in that `CertificateRequest` comes
from the delegating Java `TrustManager`'s `getAcceptedIssuers()`, snapshotted at
`SSLContext.init` time (`capture_accepted_issuer_dns`) — previously
`PassthroughClientCertVerifier` sent an empty list, which
`TestCustomSslTrustManager.testCustomTrustManagerCA` asserts against.

Observable difference from real renegotiation: one extra TCP connection, and
the request is re-sent on it. Collecting the certificate on the SAME connection
still is not possible without renegotiation support in the TLS backend.

**Residual A, by design: `testClientCertPostZero`** (1 of 18) expects
`OK-0` and gets `OK-1024`. That test sets `maxSavePostSize=0`, so during
Tomcat's renegotiation the request body cannot be buffered and the servlet sees
zero bytes. The retry emulation has no renegotiation to buffer across — it
re-sends the request, body and all, on the fresh connection, so the servlet
sees all 1024 bytes. The request still succeeds and still authenticates; only
this body-discard side effect of renegotiation differs. Closing it would
require knowing the server's `maxSavePostSize` from the client, which is not
observable. Its sibling `testClientCertPostLarger` is unaffected and passes:
there the body exceeds the buffer, Tomcat fails before `doClientAuth`, no
marker is armed and no retry happens.

### 8. `TestSsl` — two unrelated problems, neither a validation gap

**`testSni` was not an enforcement failure at all.** Instrumenting the test
showed the server correctly answers 400 for the SNI/Host mismatch the doc
quoted. The assertion that actually failed is the LAST one in the method — a
plain `getUrl("https://localhost:<port>/…")` that expects 400 because real JSSE
does **not** send the TLS `server_name` extension for a single-label host name
like `localhost` (`sun.security.ssl.Utilities.rawToSNIHostName` rejects dotless
names and IP literals). Our rustls client always sent it, so Tomcat's
`checkSni` matched the `localhost` virtual host and returned 200.

**Fix:** `jsse_would_send_sni` / `client_config_for_host` set
`ClientConfig.enable_sni = false` for dotless names and IP literals on the
`HttpURLConnection` path. The `SSLSocketFactory.createSocket` path deliberately
keeps sending SNI — see that function's comment for why.

**The class-level HANG is throughput, and is NOT closable from the TLS layer.**
`testPost` runs 8 threads that each read a 16 MiB body ONE BYTE AT A TIME —
~134 million reads. Every byte was a native call bracketed by
`begin_blocking_region`/`end_blocking_region` plus a registry lookup.

A plaintext readahead keyed by stream id (`s2_tls_fill_readahead` /
`s2_tls_pop_buffered_byte` in servlet.rs, drained at the top of `s2_tls_read`
so every reader of a stream stays consistent regardless of entry point;
`available()` now reports buffered bytes) removes the rustls and registry work
from every byte. Measured: it buys about 15% — `testPost` alone goes from
~7.4 min to 368 s. Kept, but it is not the answer.

Wrapping the stream in a real `java.io.BufferedInputStream` — the shape real
JSSE's `SSLSocketImpl$AppInputStream` has, and the obvious "make `read()` be
bytecode over a `byte[]`" fix — was tried and **made it worse**: >700 s (test
timeout) against 368 s unwrapped. A `--stack-dump-on-timeout=120` capture shows
why, and rules out the deadlock this first looked like: all 8 client threads
are inside `BufferedInputStream.read`/`fill`/`getBufIfOpen` with
`blocked=false` and a DIFFERENT pc in each dump — real progress, just far too
slow — while the Tomcat exec threads block in `doWrite` waiting for them to
drain. JDK 25's `BufferedInputStream.read()` acquires its `InternalLock` on
EVERY call, so per byte it costs an AQS lock/unlock plus interpreted bytecode,
which on this VM is dearer than the single native call it replaces. Reverted;
the reasoning is recorded at the call site in `ssl_security.rs` so it is not
re-attempted blind.

What remains is the Java->native transition cost itself (~2.7 µs/read), i.e.
the pre-existing throughput wall — see `04-embedded-server-throughput-wall-OPEN.md`
and `../../../known-issues/tomcat/29-throughput-wall-recurrence-and-unconfirmed.md`
— not a TLS defect. The multi-minute gaps between the `[OpenSSL]`
parameterisations are the same story and not a separate bug:
`testSimpleSsl[OpenSSL]` runs in **1.9 s** in isolation, so those gaps are
cumulative heap/GC state after `testPost`, not anything the OpenSSL path does.

## Residual B — `TestSSLHostConfigCompat.testHostECwithRSAandECClient`, OPEN

1 of 78, and **only in a full-class run**: executed on its own it passes in
13 s. In-class it stalls for exactly 300 s and then fails with
`SocketTimeoutException: Read timed out`. Reproduced twice.

It appeared the moment the client-restriction probe started working (root
cause #1 above) — before that the client was silently unrestricted, so this
test succeeded without exercising the path at all. With the restriction
applied the client offers exactly `{ECDHE_RSA_AES256 (aliased from DHE_RSA),
ECDHE_ECDSA_AES256}` to an EC-certificate server, which must select the ECDSA
suite. Its near-twin `testHostECwithECClient` (client offers ONLY the ECDSA
suite) passes, as does this test standalone, so the suite selection itself is
not obviously wrong — the in-class-only, exactly-300 s shape points at state
left behind by the two preceding tests that deliberately fail their handshakes
(`testHostECwithRSAClient`, `testHostRSAwithECClient`) rather than at this
test's own configuration.

Not chased further here: it is a strict improvement on the baseline (that
class went 4 failures → 1), and the alternative — leaving the probe dead —
costs `TestSSLHostConfigProtocol` a test AND leaves the general mechanism
inert. Next step for whoever picks it up: run the class with
`--stack-dump-on-timeout=90` (a standalone dump is useless, it does not
reproduce) and look for a leaked rustls stream or an exhausted connector
thread pool from the two preceding failed handshakes.

## Files touched

- `native-builtins/src/t27_tls.rs`
- `native-builtins/src/http_url_connection.rs`
- `native-builtins/src/net_phase_e.rs`
- `native-builtins/src/phases_late/ssl_security.rs`
- `native-builtins/src/servlet.rs`

## Reproduction

```powershell
.\apps\tomcat-suite-runner\run-tomcat-suite.ps1 -Category failed -RefCsv <results.csv marking these 8 non-PASS> -TimeoutSec 900 -Parallel 4 -RunName tls-repro -Exe <cratonvm.exe>
```

Real JDK 25 boot, real sockets (`CRATONVM_REAL_NET_SOCKETS=1`, set
automatically by `run-tomcat-suite.ps1` for `-Vm craton`). Compare against
`-Vm hotspot` on the same class list.

## Note: two DIFFERENT, already-fixed `TestSsl` bugs are NOT this issue

A concurrent session fixed two unrelated `TestSsl` bugs the same week (missing
`SSLSocket.addHandshakeCompletedListener` native registration;
`rustls_stream_read` not tolerating a peer closing without `close_notify`) —
see `20-fixture-completion-regressions-closure-FIXED.md`.
