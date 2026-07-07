# Netty `SelfSignedCertificate` (BouncyCastle path) → empty `X509Certificate.getEncoded()` on CratonVM

**Status:** 🔴 OPEN — isolated 2026-07-07 while fixing
[`../internal/http-server-sslengine-identity-singleton-clobber-FIXED.md`](../internal/http-server-sslengine-identity-singleton-clobber-FIXED.md).
This is the deepest of four distinct bugs behind that doc's
`ServerHttpsRequestIntegrationTests::checkUri()` "TLS handshake failed:
unexpected EOF"; the other three (RSA non-CRT `getEncoded`, the identity
singleton clobber, and `CertificateFactory.generateCertificate` empty
`getEncoded()` on PEM streams) are FIXED, which advanced the failure to this
one.
**Severity:** Medium — blocks every reactive/Netty HTTPS server test that uses
`io.netty.handler.ssl.util.SelfSignedCertificate` on a classpath that also has
BouncyCastle (bcprov/bcpkix), which is the common Spring-web test shape.

## Symptom

`io.netty.handler.ssl.util.SelfSignedCertificate().cert().getEncoded()` returns
a **0-length** byte array on CratonVM, where HotSpot returns the real ~721-byte
DER. The certificate object is also the wrong type:

| | class of `ssc.cert()` | `getEncoded().length` |
|---|---|---|
| HotSpot JDK 25 | `sun.security.x509.X509CertImpl` | 721 |
| CratonVM | `java.security.cert.X509Certificate` (bare synthetic stub) | **0** |

The bare `java.security.cert.X509Certificate` class is CratonVM's
`generateCertificate` **fallback stub** (the "input stream had no readable
bytes" branch in `native-builtins/src/phases_late.rs`, a 3-field mirror with
`CN=Unknown` and no DER) — so `generateCertificate` was reached but handed an
empty/unreadable stream.

Downstream: Netty builds a `KeyStore` and `setKeyEntry(alias, key, pw,
certChain)` with this empty-DER cert; CratonVM's `engine_set_key_entry`
extracts the (empty) cert bytes into the runtime TLS identity; rustls rejects
the server config with `invalid peer certificate: BadEncoding`, and the client
sees `unexpected EOF`.

## Repro (`NettyCertProbe`, kept out-of-tree — needs a Netty+BC classpath)

```java
import io.netty.handler.ssl.util.SelfSignedCertificate;
import java.security.cert.X509Certificate;
public class NettyCertProbe {
    public static void main(String[] a) throws Exception {
        X509Certificate cert = new SelfSignedCertificate().cert();
        System.out.println("class=" + cert.getClass().getName());
        System.out.println("len=" + cert.getEncoded().length);
    }
}
```
On the Azure spring-web test classpath (`bcprov-jdk18on`/`bcpkix-jdk18on` 1.72
+ `netty-handler` 4.2.15):
```
=== HotSpot ===   class=sun.security.x509.X509CertImpl   len=721
=== CratonVM ===  class=java.security.cert.X509Certificate   len=0
```
(A pure-JDK `CertAndKeyGen` self-signed cert, and a DER/PEM stream through
`CertificateFactory.generateCertificate`, all round-trip correctly on the same
fixed binary — see the FIXED doc's probes — so the defect is specific to the
BouncyCastle-preferred `SelfSignedCertificate` generation path.)

## Where to look (two candidate root causes, not yet distinguished)

`SelfSignedCertificate` prefers `BouncyCastleSelfSignedCertGenerator` whenever
BC is present. Netty then converts the generated cert to a JDK
`X509Certificate` (typically `CertificateFactory.getInstance("X.509")
.generateCertificate(new ByteArrayInputStream(bcCert.getEncoded()))`). The
empty result means EITHER:

1. **BC's own `X509CertificateHolder`/cert `getEncoded()` returns empty/malformed
   DER under CratonVM** (a BouncyCastle ASN.1 / provider issue — the same family
   as the `bc-crypto-regression` / X509 work), so Netty passes an empty stream;
   OR
2. **The stream Netty hands `generateCertificate` is not a
   `java.io.ByteArrayInputStream` whose buffer is field 0** (e.g. a Netty
   `ByteBufInputStream` or a wrapped stream), so CratonVM's `generateCertificate`
   native — which reads the DER from the InputStream's field-0 `buf` array —
   sees no bytes and falls back to the empty stub.

Next step: instrument `generateCertificate` to log the concrete InputStream
class + readable length when it hits the fallback, and separately probe BC's
`X509CertificateHolder.getEncoded()` directly, to decide between (1) and (2).
If (2), teach `generateCertificate` to `read()` the stream via the real
`InputStream.read([B)` bytecode instead of assuming the field-0 buffer layout.
If (1), it joins the BouncyCastle crypto-provider bug cluster.
