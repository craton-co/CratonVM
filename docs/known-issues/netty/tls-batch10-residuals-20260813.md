# netty `handler.ssl` batch 10 — residuals after the provider-routing / close-notify fixes

**Status:** OPEN (2026-08-13). What is left of
`tls-batch10-encrypted-keys-and-handshake-gaps-20260812.md` after its two root
causes were fixed and it was retired to
[`docs/internal/fixed-suite-bugs/netty-tls-batch10-provider-routing-and-close-notify-FIXED-20260813.md`][fixed].

[fixed]: ../../internal/fixed-suite-bugs/netty-tls-batch10-provider-routing-and-close-notify-FIXED-20260813.md

Everything on this page is a **different defect** from the two that were fixed
(CratonVM discarding an explicitly requested JCA `Provider`, and `wrap` never
emitting a queued alert). They were surfaced by the same triage, not caused by
the same thing, and none of them is an encrypted-key or alert-delivery problem.

## Where the batch stands

CratonVM-specific failures only — the environment is subtracted, since
netty-tcnative is absent on this host and its `UnsatisfiedLinkError`s land on
HotSpot too.

| class | HotSpot 25 | before | after |
|---|---|---|---|
| `SniHandlerTest` | 18 ok | 11 ok / **7 failed** | 18 ok |
| `CloseNotifyTest` | 2 ok / 2 aborted | 0 ok / **2 failed** | 2 ok |
| `ParameterizedSslHandlerTest` | 7 ok, 10 s | **HANG** (>17 min) | 7 ok, 8 s |
| `ApplicationProtocolNegotiationHandlerTest` | 8 ok | 6 ok / **2 failed** | 8 ok |
| `JdkSslServerContextTest` | 35 ok / 1 aborted | 26 ok / **9 failed** | 33 ok / **2 failed** |
| `JdkSslClientContextTest` | 34 ok / 1 aborted | 24 ok / **10 failed** | 29 ok / **5 failed** |
| `SslContextBuilderTest` | 9 ok / 9 failed | 6 ok / **12 failed** | 6 ok / **12 failed** |
| `SniClientTest` | 2 ok / 1 failed | 1 ok / **2 failed** | 1 ok / **2 failed** |

34 CratonVM-specific failures plus one hang, down to 11 and none.

## R1 — `TrustManagerFactorySpi` dispatch resolves against `KeyStore`

3 failures, all `JdkSslClientContextTest`
(`testSslContextWithUnencryptedPrivateKey`,
`testSslContextWithEncryptedPrivateKey`,
`testSslContextWithEncryptedPrivateKey2`):

```
java.lang.NoSuchMethodError: java.security.KeyStore.engineGetTrustManagers()[Ljavax/net/ssl/TrustManager;
```

`engineGetTrustManagers` is declared on `javax.net.ssl.TrustManagerFactorySpi`,
not on `java.security.KeyStore` — so a receiver is being resolved against the
wrong class. The mis-typed receiver is the thing to find; the method exists.

## R2 — PBES2 `AlgorithmParameters` decoding

2 failures, `testPkcs8Des3EncryptedRsa` on both context tests:

```
java.io.IOException: PBE parameter parsing error: expecting the object identifier for AES cipher
```

This message is the **real JDK's own** (`com.sun.crypto.provider.PBES2Parameters`),
so the DER it was handed does not carry the encryption-scheme OID it expects.
`rsa_pkcs8_des3_encrypted.key` is PBES2 with **DESede** as the scheme, and the
message says the parser only accepted an AES OID — worth checking whether
CratonVM re-encodes the parameters anywhere on the way in, and whether the same
file parses when BouncyCastle handles it (it does not reach BC today: netty
only falls back to the JDK path, so this is on the fallback).

## R3 — `SslContextBuilder` does not use a caller-supplied `SecureRandom`

2 failures, `SslContextBuilderTest.testServerContextWithSecureRandom` /
`testClientContextWithSecureRandom`, both `expected: <true> but was: <false>`.
The test supplies its own `SecureRandom` and asserts it was actually drawn
from. CratonVM's TLS stack is rustls-backed and takes randomness from `ring`,
so a caller-supplied `SecureRandom` is silently ignored — the same shape of
defect as the provider routing above, one layer over.

## R4 — combined cert+key PEM

1 failure, `SslContextBuilderTest.testCombinedPemFileClientContextJdk`:

```
java.lang.IllegalArgumentException: Input stream does not contain valid private key.
```

A single PEM file holding both the certificate and the key. The reader finds no
key in it.

## R5 — a `TrustManager` that throws an `Error`

1 failure beyond HotSpot's own, `SniClientTest`. The test's `TrustManager`
throws `org.opentest4j.AssertionFailedError` — an `Error`, not an `Exception` —
and `t27_tls::engine_run_trust_check` converts every `Throwable` it catches
into `SSLHandshakeException`:

```
javax.net.ssl.SSLHandshakeException: TrustManager rejected the peer certificate chain: org/opentest4j/AssertionFailedError:
```

JSSE does not catch `Error`s there; a JUnit assertion failure inside a
`TrustManager` is meant to reach the test runner intact. The fix is to let
`java.lang.Error` propagate unchanged and convert only `Exception`s. Note this
test method fails on HotSpot too (a port collision), so closing R5 changes the
cause, not necessarily the count.

## R6 — `JdkSslEngineTest`: not a hang, and a 481-failure residual

The retired page recorded this class as `rc=124` and wondered whether "at 178 s
on HotSpot it may need nothing but a bigger cap". It needs a bigger cap **and**
it is the largest single block of unexplained failures left in this suite.

Run to completion on an idle host, 2026-08-13:

| | tests | ok | failed | aborted | wall |
|---|---|---|---|---|---|
| HotSpot 25 | 821 | 755 | **0** | 66 | 96 s |
| CratonVM | 821 | 274 | **481** | 66 | 1984 s |

So: **not a hang** — it finishes, and the earlier 600 s / 1200 s caps were
simply too small. Both VMs abort the same 66 (netty-tcnative absent). What is
left is 481 CratonVM failures against HotSpot's zero, and a **20.7x**
wall-clock gap.

Two separate questions live here, and they should not be conflated:

* **Correctness.** 481 failures across a parameterised matrix of
  `{Heap, Direct, Mixed} x {TLSv1.2, TLSv1.3} x {cipher} x {delegate}`. The
  parameterisation is the lever: comparing which *axis values* fail should
  collapse this to a small number of causes rather than 481.
* **Throughput.** 20.7x is not explained by the failures — a failing test is
  usually faster, not slower. Measure it separately.

Nothing on this page is a prerequisite for that work; this class was never part
of the two root causes that were fixed.

## Repro (Linux host)

```bash
cd /data/cratonvm/apps/netty-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv-bin> --java-home "$JAVA_HOME" --Xmx 1500m \
    @common.args -Dcraton.batch=1 CratonRunner io.netty.handler.ssl.JdkSslClientContextTest
```
