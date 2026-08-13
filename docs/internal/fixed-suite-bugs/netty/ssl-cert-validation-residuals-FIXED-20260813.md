# `handler.ssl`/OCSP certificate-validation residuals — FIXED 2026-08-13

**Status:** ✅ FIXED for every row. Retired from `docs/known-issues/netty/`.
The doc was a raw evidence dump that explicitly refused to claim a shared cause;
it turned out four of the seven rows *did* share one (`OpenSsl.isAvailable()`
was permanently false), and the remaining three were three separate, small
defects. Residual per-test failures that survive are a different family and are
tracked in
`docs/known-issues/netty/openssl-key-material-and-engine-residuals-20260813.md`.

## Result

Re-measured on Azure host 2 (Linux x86_64, JDK 25) from `origin/dev` @
`c4c972da7`, with `netty-tcnative-boringssl-static` on the classpath so both VMs
have the same OpenSSL capability the original Windows run had.

| class | before | after | HotSpot 25 |
|---|---|---|---|
| `SslContextBuilderTest` | 9 ok / 9 f / 3 a | **20 ok / 1 f** | 21 ok |
| `SslHandlerTest` | 30 ok / 22 f | **45 ok / 8 f / 1 a** | 53 ok / 1 a |
| `ocsp.OcspClientTest` | 4 ok / 2 f | **6 ok** ✅ | 6 ok |
| `ocsp.OcspServerCertificateValidatorTest` | 0 ok / 1 f | **1 ok** ✅ | 1 ok |
| `OpenSslKeyMaterialManagerTest` | 0 ok / 1 f | **1 ok** ✅ | 1 ok |
| `PemEncodedTest` | 1 ok / 2 f | **1 ok / 2 a** ✅ | 1 ok / 2 a |
| `CloseNotifyTest` | 2 ok / 2 a | **4 ok** ✅ | 4 ok |

## The four causes

### 1. `OpenSsl.isAvailable()` was false — `SslContextBuilderTest`, `CloseNotifyTest`, `OpenSslKeyMaterialManagerTest`, `PemEncodedTest`, and most of `SslHandlerTest`

`UnsatisfiedLinkError: failed to load the required native library` at
`OpenSsl.ensureAvailability`. CratonVM refused to run netty's `netty_tcnative`
`JNI_OnLoad` and refused to resolve `io/netty/internal/tcnative/**` symbols. The
full chain — and the three further defects that had to be fixed before the real
library actually worked — is in the retired
`ssl-suite-test-discovery-undercounts` write-up, which this doc's sibling
covered. The environment note the original doc flagged was right for the right
reason: HotSpot had gained tcnative, CratonVM never could.

### 2. `CertStore.Collection` was not a registered SUN service — `OcspClientTest`, `OcspServerCertificateValidatorTest`

`java.security.NoSuchAlgorithmException: Collection CertStore not available`,
which BouncyCastle re-wraps as the misleading `OCSPException: Error setting up
certificate path validation` — the exact signature the original doc recorded.
CratonVM seeded `CertPathBuilder.PKIX` and `CertPathValidator.PKIX` but not the
`CertStore` those need to find intermediates. Fixed in
`native-builtins/src/jca/provider_chain.rs`.

Two further defects sat behind it:

* **`CertStoreSpi` has no no-arg constructor.** `Provider$Service.newInstance`
  always called `()V`; JCA passes the `CertStoreParameters` to a one-argument
  ctor for this engine. `jca_service_ctor_parameter_type` now names the
  exception.
* **`Provider$Service`'s slot mirror corrupted a real JDK `Service`.** The
  synthetic layout is `(type=0, algorithm=1, provider=2, className=3)`; the real
  JDK's declaration order is `(provider=0, type=1, algorithm=2, className=3)`.
  Writing the synthetic mirror onto a real `Service` rotated all three by one
  slot and undid the `set_field_by_name` writes above it, so
  `CertStore.getProvider()` returned the *type* String and `.getName()` on it
  threw `NoSuchMethodError: java.lang.String.getName()`. It stayed invisible
  because every other JCA engine CratonVM serves is intercepted upstream of the
  JDK's own `GetInstance`; `CertStore` is not. The mirror now runs only when the
  named write did not land.

### 3. `HttpsURLConnection`'s TLS accessors were abstract — `OcspClientTest`

`AbstractMethodError: javax/net/ssl/HttpsURLConnection.getServerCertificates()
has no Code attribute`. CratonVM's HTTP carrier registers the
`HttpURLConnection` surface but never registered the HTTPS-only accessors, so
the call landed on the abstract declaration. `getServerCertificates`,
`getLocalCertificates`, `getCipherSuite`, `getPeerPrincipal` and
`getLocalPrincipal` are now bridged from the peer chain the `perform` handshake
already had in hand (`native-builtins/src/http_url_connection.rs`).

A second half was needed: CratonVM's `connect()` is deliberately a no-op for a
real-JDK carrier (HotSpot's `connect()` opens the socket but sends nothing, so
the request is deferred to `getResponseCode`/`getInputStream`). An app that does
`connect(); getServerCertificates();` — which this test does — therefore had no
handshake behind it. The accessors now drive the same lazy exchange the response
getters drive.

Also fixed in passing: the https branch reported rustls's `Debug` spelling of the
cipher suite (`TLS13_AES_256_GCM_SHA384`) where JSSE drops the `13` infix. It now
goes through `t27_tls`'s existing `suite_to_java_cipher_name`.

### 4. PKCS#12 aliases were the hex `localKeyId`, not `getUnfriendlyName()`

Found while re-checking the newly-running OpenSSL classes, not from a row of this
doc, but it belongs with it. CratonVM's PKCS#12 reader named a bag with no
`friendlyName` after the hex of its `localKeyId` — what *keytool prints*, not
what `sun.security.pkcs12.PKCS12KeyStore` assigns. The reader's
`getUnfriendlyName()` is `++counter` as a decimal string, shared across every
entry kind in one load. Measured on netty's own `mutual_auth_server.p12`
(a `localKeyID`, no `friendlyName`): HotSpot reports alias `1`, CratonVM reported
`70e4faffe3f82e2db1c949282bf86a7c17348838`, and every caller that looks an entry
up by the alias its own test data documents missed. Fixed in
`native-builtins/src/keystore.rs`.

## The one row that was NOT a shared cause

The original doc grouped `BouncyCastleUtilTest` with the OCSP classes on the
grounds that all three moved when BouncyCastle appeared on the classpath. A
concurrent session root-caused that one separately — `Security.getProvider`
returned a stand-in rather than the registered `Provider` object, so
`BouncyCastleUtil`'s `instanceof BouncyCastleProvider` test failed — and it is
fixed (2/2) under the retired
`bouncycastleutiltest-security-getprovider-identity` write-up. It shares no
cause with the rows above: the OCSP failures were a missing
`CertStore.Collection` service and an abstract `getServerCertificates()`.

## What is left

`SslContextBuilderTest`'s `testInvalidCipherJdk` (an `IllegalArgumentException`
that is not thrown) and `SslHandlerTest`'s 8 remaining failures — including
`testHandshakeFailureCipherMissmatchTLSv12Jdk`/`TLSv13Jdk`, where CratonVM's JDK
engine closes the channel instead of raising `SSLException` — are recorded in
`docs/known-issues/netty/openssl-key-material-and-engine-residuals-20260813.md`
together with the `KEY_VALUES_MISMATCH` family the OpenSSL half exposed.

## Repro

```bash
cd apps/netty-suite-runner
printf '%s\n' io.netty.handler.ssl.SslContextBuilderTest io.netty.handler.ssl.SslHandlerTest io.netty.handler.ssl.ocsp.OcspClientTest io.netty.handler.ssl.ocsp.OcspServerCertificateValidatorTest io.netty.handler.ssl.OpenSslKeyMaterialManagerTest io.netty.handler.ssl.PemEncodedTest io.netty.handler.ssl.CloseNotifyTest > /tmp/ssl-residuals.txt
CV_BIN=bin/cratonvm-netty-zgc bash run-netty-suite.sh --list /tmp/ssl-residuals.txt --gc zgc --shards 1 --timeout 400 --out /tmp/repro
```

`common.args` must carry `netty-tcnative-boringssl-static-<ver>-<os>.jar`;
without it `OpenSsl.isAvailable()` is false on both VMs and the comparison says
nothing. The two OCSP classes reach the public internet (`apple.com`), so they
need outbound 443/80.
