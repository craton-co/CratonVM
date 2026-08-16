# A default-`SSLContext` client rejects certificates HotSpot accepts: OpenSSL's SECLEVEL is deciding trust, not the JDK's TrustManager

## Status
**OPEN, found 2026-08-16.** Differential-verified against Temurin 25.0.3 on the
Azure host (`azureuser@20.80.105.49`) with a two-line probe. No test's verdict
is known to turn on it yet — it was found while closing the H2 `TestTools` TLS
residual, where both VMs reject the certificate anyway because H2 configures no
trust store. It is filed because the direction is the dangerous one: CratonVM
**refuses a connection HotSpot completes**, so it can only ever turn a passing
app into a failing one.

## Measured

`TlsProbe3` — H2's own `NetUtils` on both ends, but with H2's baked keystore
installed as the **trust store** as well, so the server's certificate is
explicitly trusted:

```
CERT alias=h2 keyAlg=RSA sigAlg=MD5withRSA subject=CN=H2 selfSigned=true

HOTSPOT  : CLIENT(trust store installed) ms=197 -> TRUSTED-HANDSHAKE-OK
           SERVER read -> 7
CRATONVM : CLIENT(trust store installed) ms=50  -> REJECTED
           javax.net.ssl.SSLHandshakeException: TLS handshake failed:
           error:0A000086:SSL routines:tls_post_process_server_certificate:
           certificate verify failed:../ssl/statem/statem_clnt.c:1889:
           (EE certificate key too weak)
           SERVER: … sslv3 alert bad certificate … SSL alert number 42
```

Same certificate, same trust store, same process shape. HotSpot completes the
handshake and reads the byte; CratonVM sends `bad_certificate`.

## Why they differ

The two VMs are applying different rule sets, and neither is "wrong" on its own
terms:

* **HotSpot** runs the chain through `sun.security.validator.PKIXValidator`,
  whose algorithm constraints come from `jdk.certpath.disabledAlgorithms`.
  A certificate installed as a **trust anchor** is exempt from the
  signature-algorithm check — which is why an `MD5withRSA` self-signed
  certificate that the user has explicitly chosen to trust is accepted.
* **CratonVM** hands verification to the native backend. The `native_tls` /
  OpenSSL client applies OpenSSL's **security level** (SECLEVEL 2 by default:
  RSA below 2048 bits, and MD5/SHA-1 signatures, are refused), and OpenSSL
  applies it to the peer certificate whether or not the user trusts it.

So the extra strictness is not a policy CratonVM chose; it is the backend's
policy leaking through where the JDK's should be authoritative.

Note this is also the reason CratonVM's rejection *message* can never match
HotSpot's on an untrusted certificate: HotSpot gets as far as path building and
says "unable to find valid certification path", while CratonVM stops earlier at
"EE certificate key too weak". Same verdict, different reason, and the reason
CratonVM gives is the one that would still fire after the trust problem was
fixed.

## Not simply "lower the security level"

Dropping SECLEVEL on the client path would remove the strictness AND remove
every check the JDK does keep — this path has no other verifier when the
`SSLContext` carries no explicit `TrustManager`. The client would then accept
things HotSpot refuses, which is a worse bug than the one being fixed.

## What a fix looks like

CratonVM already has the right machinery for the case where the application
supplies TrustManagers: `new13_connect_and_handshake_on`
(`native-builtins/src/phases_late/ssl_security.rs`) disables native
verification when `java_tm_key` is set, captures the peer chain, and calls
`t27_tls::run_client_trust_check_for_chain`, failing closed. That is exactly
the "let the TrustManager decide" shape this needs.

The gap is the **default** context — `SSLSocketFactory.getDefault()`, which is
what most application code and every `javax.net.ssl.trustStore`-configured
deployment uses. Real JSSE builds a default `X509TrustManager` there
(`TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm())`
initialised with `null`, which picks up `javax.net.ssl.trustStore` or the JDK's
`cacerts`). Doing the same — so the default path also routes the decision
through the JDK's own bytecode instead of OpenSSL's SECLEVEL — is the
principled fix.

It is deliberately NOT bundled with the H2 fix that found it: it changes the
verification path for **every** plain client TLS connection in the VM (Spring,
netty, the JDK HttpClient shims), so it wants its own branch and its own gate
runs across those suites.

## Repro
```bash
cd apps/h2database/h2
CP="target/classes:target/test-classes:$(cat craton-testcp.txt)"
$JDK25/bin/java -cp "$CP:<probe-dir>" TlsProbe3 9611          # TRUSTED-HANDSHAKE-OK
<cratonvm-bin> --java-home $JDK25 --nojit -c "$CP:<probe-dir>" TlsProbe3 9621   # REJECTED
```
`TlsProbe3` writes `CipherFactory.getKeyStore(CipherFactory.KEYSTORE_PASSWORD)`
to a file, points `javax.net.ssl.keyStore` AND `javax.net.ssl.trustStore` at it
before any SSL use, then runs one H2 loopback SSL exchange.

## Related
* The retired
  `bug-h2-suite-fail-cluster-pgserver-tools-memoryunmapper-filelock-timer-20260807`
  write-up — where this was found, and whose §2b claim "both VMs reject the
  same certificate; only the report differs" this corrects.
