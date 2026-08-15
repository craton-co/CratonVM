# netty OpenSSL key material: the opaque-key gap, plus three JDK-engine residuals

**Status:** OPEN and smaller again. Section A (`KEY_VALUES_MISMATCH`) was fixed
2026-08-14. Section C is fixed and section A′'s `AbstractMethodError` is gone as
of 2026-08-15, which moved `OpenSslPrivateKeyMethodTest` from 0/24 to 24/24 and
uncovered a NEW, deeper A′ residual. B and D are unchanged in kind, smaller in
degree.

Measured on Azure host 2 (Linux x86_64, JDK 25), one class per process,
`-XX:+UseG1GC`, with `netty-tcnative-boringssl-static` on the classpath for
BOTH VMs. G1 was used because ZGC was aborting several of these classes at the
time — since fixed on dev. If a class in this suite aborts inside `gc/`, check
that first rather than reading it as a TLS defect.

| class | dev 2026-08-15 | this page's work | HotSpot 25 |
|---|---|---|---|
| `JdkDelegatingPrivateKeyMethodTest` | 0 / 27 | **1 / 27** | 27 / 27 |
| `OpenSslPrivateKeyMethodTest` | 0 / 24 | **24 / 24** | 3 / 24 |
| `SslHandlerTest` | 46 / 54 | **47 / 54** | 53 / 54 |
| `SslContextBuilderTest` | 20 / 21 | **21 / 21** ✅ | 21 / 21 |
| `ParameterizedSslHandlerTest` | never finished | 60/63 in 124 s, or hangs | 63 / 63 |

`OpenSslPrivateKeyMethodTest` now passes 24 where HotSpot passes 3 — that
direction is a claim to check, not a win to bank. HotSpot's 21 failures on this
host are netty-tcnative's own; the number is recorded here so the next reader
does not mistake the gap for progress.

## What was section A — FIXED 2026-08-14

`Error setting certificate (…:KEY_VALUES_MISMATCH)` and its sibling `Unable to
load certificate key (error:00000000:invalid library (0))` were one defect: a
**KeyManager** id and a **keystore** id, from independent counters, sharing
field 3 of one four-slot `java/security/PrivateKey` proxy. Fixed by tagging the
composite (`KM_PROXY_TAG`, bit 62). Landed with
`KeyManagerFactory.getInstance("PKIX"|"NewSunX509")` handing out
`sun.security.ssl.X509KeyManagerImpl` rather than reporting `SunX509` for every
algorithm.

## A′ — the `AbstractMethodError` is fixed; a signature-length defect is behind it

**What it was.** `KeyManagerFactory.getKeyManagers()` answered with
`try_alloc_concurrent_synthetic("javax/net/ssl/X509KeyManager", 0)` whenever
`keystore_id_from_object` returned 0 — an object stamped with the INTERFACE's
class id, every method abstract. netty reaches that branch because
`OpenSslX509KeyManagerFactory.newKeyless` builds its OWN `KeyStore` subclass
over a hand-written `KeyStoreSpi`, and netty's factory SPI then inits a DEFAULT
`KeyManagerFactory` with it. `OpenSslKeyMaterialProvider.chooseKeyMaterial`
called `getCertificateChain(alias)` on the result and died.

**What it is now.** `x509_manager::build_key_manager_state_from_live_keystore`
enumerates ANY `KeyStore` through its own bytecode (`aliases()` /
`getCertificateChain()` / `getKey()`) and builds a real, natively-backed
manager in the same `km_registry` id space. An *opaque* key — one whose
`getEncoded()` is null, which is the entire point of
`OpenSslPrivateKeyMethod` — is kept BY REFERENCE in
`KeyManagerState::aliases_to_live_key` (GC-rooted through
`gc_scan_key_manager_roots`) and handed back unchanged by `getPrivateKey`,
because `chooseKeyMaterial` branches on `key instanceof OpenSslPrivateKey`.
The `ks_id == 0` fallback is now an EMPTY natively-backed mirror, not a
bare-interface object: `getCertificateChain` returning null is contract-legal,
`AbstractMethodError` is not.

**The residual (25 of the 26 remaining failures).**

```
io.netty.handler.ssl.ReferenceCountedOpenSslEngine$OpenSslHandshakeException:
  error:04000070:RSA routines:OPENSSL_internal:DATA_LEN_NOT_EQUAL_TO_MOD_LEN
  → TLSV1_ALERT_DECRYPT_ERROR
```

The key material now reaches OpenSSL and the handshake gets as far as the
CertificateVerify. The signature that `JdkDelegatingPrivateKeyMethod` produces
through `Signature.getInstance(alg, provider)` is not modulus-length. That is a
JCA `Signature` defect, not a TLS one: start by printing the algorithm name
netty asks for, the provider that answers, and `sign()`'s output length, for
each of the eleven `supportedAlgorithms()` parameterisations.

## B — cipher-mismatch handshakes close instead of raising (2 of 54)

`SslHandlerTest.testHandshakeFailureCipherMissmatchTLSv12Jdk` / `TLSv13Jdk`:

```
expected: <javax.net.ssl.SSLException>
but was:  <io.netty.channel.StacklessClosedChannelException>
```

At ENGINE level this is already right — a client and server restricted to
disjoint suites now raise `SSLHandshakeException: rustls: received fatal alert:
HandshakeFailure` on both sides (probe it directly rather than through the
handler). What is left is the `SslHandler`/`EmbeddedChannel` path, where the
failing side closes the channel before the alert reaches the peer's pipeline.

`SslHandlerTest`'s other four: `testTruncatedPacket` (expects a
`DecoderException`, gets none), `testHandshakeFailureOnlyFireExceptionOnce`, and
the `testHandshakeTimeoutBecauseExecutorNotExecute` pair (expect an
`SslHandshakeTimeoutException`, get null) — all unexamined.

## ~~C~~ — `SslContextBuilder` accepts an invalid cipher — FIXED 2026-08-15

`SSLEngine.setEnabledCipherSuites` validates its argument now and throws
`IllegalArgumentException` for a name that is not a cipher suite, which is what
netty's `JdkSslContext` relies on both to fail loudly and as a probe.
`SslContextBuilderTest` is 21/21.

## D — `ParameterizedSslHandlerTest` intermittently hangs

Still intermittent: 60/63 in 124 s on one build, hung on the next, both with
the same binary shape. It always stops in the same place — an OPENSSL↔OPENSSL
NIO-channel parameterisation (`4:`/`5: clientProvider=OPENSSL_REFCNT`), with
the event loops quiet afterwards, so the promise it is waiting on is never
completed.

**One contributing cause is fixed and one premise was wrong.** The
`Selector.select() returned prematurely 512 times in a row; rebuilding Selector`
storm (687 rebuilds in a baseline run, self-sustaining) came from
`nio_selector.rs`'s interest-ops **nudge**: a byte written to the wakeup
self-pipe after `epoll_ctl(EPOLL_CTL_MOD)`, on the premise that "epoll_ctl(MOD)
does not reliably interrupt an already-blocked epoll_wait". On Linux that
premise is false — `ep_modify()` re-evaluates readiness against the new mask
and wakes the waiters — while the byte guaranteed a zero-key return, which is
exactly what netty counts. Removed for Linux (Windows keeps its own: WSAPoll
genuinely cannot see the change). The storm is gone; **the hang is not**, which
means the storm was a symptom rather than the cause.

Next step is a thread dump at the stall, not more selector reasoning: re-run
WITHOUT `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1` so the watchdog dumps, and read
which thread owns the incomplete promise.

## Repro

```bash
cd apps/netty-suite-runner
printf '%s\n' io.netty.handler.ssl.JdkDelegatingPrivateKeyMethodTest \
  io.netty.handler.ssl.OpenSslPrivateKeyMethodTest \
  io.netty.handler.ssl.SslHandlerTest io.netty.handler.ssl.SslContextBuilderTest > /tmp/km.txt
CV_BIN=bin/cratonvm-netty-zgc bash run-netty-suite.sh --list /tmp/km.txt --gc g1 --shards 1 --timeout 400 --out /tmp/repro
```

`common.args` MUST carry `netty-tcnative-boringssl-static-<ver>-<os>.jar`.
Without it `OpenSsl.isAvailable()` is false, the OPENSSL parameters are never
generated, and these classes read as clean passes while running a fraction of
their tests. The netty Maven reactor resolves the *dynamic* `netty-tcnative`
artifact on Linux, whose `.so` needs `OPENSSL_3.2.0`; on a host with an older
`libssl.so.3` neither VM can load it.

`CRATONVM_DBG_TLS_AUTH=1` prints the `KeyManagerFactory.init` keystore id and,
now, the alias count of a live-enumerated `KeyStore`.

## Related

- retired `ssl-suite-test-discovery-undercounts` and
  `ssl-cert-validation-residuals` write-ups — the work that made these reachable.
- `docs/known-issues/netty/jdksslenginetest-engine-level-gaps-20260813.md` —
  JDK-engine gaps in the same package, 307 → 48 failures in the same session.
