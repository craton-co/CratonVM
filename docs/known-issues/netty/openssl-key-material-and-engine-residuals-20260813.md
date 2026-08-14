# netty OpenSSL key material: `KEY_VALUES_MISMATCH`, plus two JDK-engine residuals

**Status:** OPEN, one signature root-caused only as far as "the Java side is
provably correct" (2026-08-13). Found on Azure host 2 (Linux x86_64, JDK 25),
branch built from `origin/dev` @ `c4c972da7`, `--shards 4 --timeout 400`, with
`netty-tcnative-boringssl-static` on the classpath for BOTH VMs.

These are **newly reachable**, not new regressions. Until 2026-08-13 CratonVM
refused to load netty's `netty_tcnative` library, so `OpenSsl.isAvailable()` was
false and 89–100 % of these classes' test instances were never generated (the
retired `ssl-suite-test-discovery-undercounts` write-up). Making the real
library work exposed the OpenSSL half of the suite for the first time.

## A — `Error setting certificate (…:KEY_VALUES_MISMATCH)`

BoringSSL rejects the (certificate, private key) pair netty hands it, from
`OpenSslKeyMaterialManager.setKeyMaterial` inside the tcnative certificate
callback:

```
javax.net.ssl.SSLHandshakeException: General OpenSslEngine problem
  Caused by: javax.net.ssl.SSLException: java.lang.Exception: Error setting certificate
    (error:0b000074:X.509 certificate routines:OPENSSL_internal:KEY_VALUES_MISMATCH)
    at io.netty.handler.ssl.ReferenceCountedOpenSslEngine.setKeyMaterial(…:454)
    at io.netty.handler.ssl.OpenSslKeyMaterialManager.setKeyMaterial(…:143)
    at io.netty.handler.ssl.OpenSslKeyMaterialManager.setKeyMaterialServerSide(…:82)
    at io.netty.handler.ssl.ReferenceCountedOpenSslServerContext$OpenSslServerCertificateCallback.handle(…:251)
    at io.netty.internal.tcnative.CertificateCallbackTask.runTask(…:40)
```

| class | CratonVM | HotSpot 25 |
|---|---|---|
| `SniHandlerTest` | 50 found, 40 ok, **10 f** | 50 ok |
| `JdkDelegatingPrivateKeyMethodTest` | 27 found, **0 ok** | 27 ok |
| `OpenSslPrivateKeyMethodTest` | 24 found, **0 ok** | 24 found, 3 ok |
| `PkiTestingTlsTest` | 13 found, 3 ok, **10 f** | 13 ok |
| `OpenSslCachingKeyMaterialProviderTest` | 6 found, 3 ok, **3 f** | 6 ok |
| `OpenSslX509KeyManagerFactoryProviderTest` | 6 found, 3 ok, **3 f** | 6 ok |
| `ocsp.OcspTest` | 14 found, 13 ok, **1 f** | 14 ok |

(`OpenSslPrivateKeyMethodTest` fails 21/24 on HotSpot too — only the 3-ok gap is
CratonVM's. The last two classes were 2 ok each until the PKCS#12
`getUnfriendlyName()` alias fix landed on 2026-08-13; their sibling
`OpenSslKeyMaterialProviderTest` went 1 ok → 2 ok and now matches HotSpot, so
that alias defect is fixed and is NOT what is left here.)

### Ruled out — do not re-check these

Each was measured on both VMs with the same inputs and agrees byte-for-byte:

* **The private key netty derives.** `SslContext.toPrivateKey(test_encrypted.pem,
  "12345")` yields a key whose modulus equals `test.crt`'s public modulus, PKCS#8
  SHA-256 `e66fa965…`, identical on both VMs.
* **The certificate chain.** `SslContext.toX509Certificates(test.crt)` gives one
  certificate, same DER hash, same subject/issuer, on both.
* **The keystore round-trip.** `KeyStore.getInstance("PKCS12")` +
  `setKeyEntry(alias, key, pw, chain)` + `getKey`/`getCertificateChain` returns
  the same bytes on both (CratonVM's `getKey` returns a synthetic `PrivateKey`
  carrier rather than `RSAPrivateCrtKeyImpl`, but `getEncoded()` matches).
* **`KeyManagerFactory` pairing, including across two independent keystores in
  one process.** `chooseEngineServerAlias("RSA", …)` → `getPrivateKey` +
  `getCertificateChain` pairs correctly for each, and stays correct when a second
  keystore with a different pair is created between the two lookups.
* **netty's own base64.** `SslUtils.toBase64` (which builds the PEM handed to the
  BIO) produces byte-identical output to HotSpot's for 3/4/5/64/770/1219/4096-byte
  inputs.
* **The `Unsafe`-arena address path.** `-Dio.netty.noUnsafe=true` and
  `-Dio.netty.maxDirectMemory=0` change nothing, and the JNI `jlong`-argument
  translation landed with this same change set.
* **netty's own switches.** `io.netty.handler.ssl.openssl.useKeyManagerFactory=false`
  and `…openssl.useTasks=false` change nothing; the stack still runs through
  `CertificateCallbackTask`.
* **Context construction.** Building the same `SslProvider.OPENSSL` server
  context 8 times in one process succeeds every time; the failure is
  per-handshake, in the certificate callback, not at build.

### Where to look next

BoringSSL parses **both** blobs successfully and only then finds they disagree,
so the BIO content is valid-but-wrong rather than corrupt. That points at the
native side of `OpenSslKeyMaterialProvider` — `SSL.parseX509Chain(chainBio)` /
`SSL.parsePrivateKey(keyBio, password)` and the `long` handles they return,
which netty then releases via `OpenSslKeyMaterial.release()`. A handle freed
early and reused would present exactly this shape: a different, valid key. That
makes netty's reference counting (`AbstractReferenceCounted`, an
`AtomicIntegerFieldUpdater` on a `refCnt` stored as `value >>> 1`) the first
thing to instrument — CratonVM's atomics moved to hardware primitives the same
day this was measured. Break on `netty_internal_tcnative_SSL_setKeyMaterial` and
dump the two objects rather than reasoning from Java.

## B — `HttpsURLConnection` cipher-mismatch handshakes close instead of raising

`SslHandlerTest.testHandshakeFailureCipherMissmatchTLSv12Jdk` /
`TLSv13Jdk` (JDK provider, no OpenSSL involved):

```
org.opentest4j.AssertionFailedError: Unexpected type,
  expected: <javax.net.ssl.SSLException>
  but was:  <io.netty.channel.StacklessClosedChannelException>
  at io.netty.handler.ssl.SslHandlerTest.testHandshakeFailureCipherMissmatch(…:1673)
```

CratonVM's JDK `SSLEngine` closes the channel on a cipher-suite mismatch where
JSSE raises an `SSLException` carrying the `handshake_failure` alert. HotSpot
passes both.

## C — `SslContextBuilder` accepts an invalid cipher

`SslContextBuilderTest.testInvalidCipherJdk`: `assertThrows(IllegalArgumentException.class, …)`
gets nothing. CratonVM does not reject an unknown cipher-suite name where the
JDK provider does. One test; HotSpot passes.

## D — `ParameterizedSslHandlerTest` intermittently hangs in a selector spin

`ParameterizedSslHandlerTest` is 63/63 ok when it finishes, and finishes in
150–190 s. On a loaded host it hangs instead — 3 of 6 runs, at 400 s, 900 s and
1500 s caps. Traced with a per-test start/finish listener, it always stops at the
same point:

    reentryOnHandshakeCompleteNioChannel
      | 5: clientProvider=OPENSSL_REFCNT, 5: serverProvider=OPENSSL_REFCNT

and the log then repeats, forever:

    io.netty.channel.nio.NioIoHandler - Selector.select() returned prematurely
      512 times in a row; rebuilding Selector sun.nio.ch.SelectorImpl@…
    io.netty.channel.nio.NioIoHandler - Migrated 1 channel(s) to the new Selector

JUnit's 120 s per-test timeout never fires, because
`SameThreadTimeoutInvocation` can only check after the invocation returns.

This is the NIO-channel variant — a real socket pair, not `EmbeddedChannel` —
so it is a CratonVM `Selector` behaviour question (a `select()` that returns
with no ready keys and no timeout elapsed), not an OpenSSL one; the OPENSSL_REFCNT
pairing is what makes the handshake slow enough to sit in the selector loop.
Whether the same spin underlies the four `*OpenSslEngine*` classes that now hang
at 400 s is unknown — HotSpot also hangs on those four at that cap, so they need
a raised per-class timeout before they can be judged.

## Repro

```bash
cd apps/netty-suite-runner
printf '%s\n' io.netty.handler.ssl.SniHandlerTest io.netty.handler.ssl.JdkDelegatingPrivateKeyMethodTest io.netty.handler.ssl.PkiTestingTlsTest io.netty.handler.ssl.SslHandlerTest io.netty.handler.ssl.SslContextBuilderTest > /tmp/km.txt
CV_BIN=bin/cratonvm-netty-zgc bash run-netty-suite.sh --list /tmp/km.txt --gc zgc --shards 1 --timeout 400 --out /tmp/repro
```

`common.args` MUST carry `netty-tcnative-boringssl-static-<ver>-<os>.jar`.
Without it `OpenSsl.isAvailable()` is false, the OPENSSL parameters are never
generated, and every class in section A reads as a clean pass while running a
fraction of its tests — which is the reporting trap the retired
`ssl-suite-test-discovery-undercounts` write-up was about. Note that the netty
Maven reactor resolves the *dynamic* `netty-tcnative` artifact on Linux, whose
`.so` needs `OPENSSL_3.2.0`; on a host with an older `libssl.so.3` neither VM
can load it.

## Related

- retired `ssl-suite-test-discovery-undercounts` and
  `ssl-cert-validation-residuals` write-ups — the work that made these reachable.
- `docs/known-issues/netty/jdksslenginetest-engine-level-gaps-20260813.md` —
  JDK-engine gaps in the same package.
