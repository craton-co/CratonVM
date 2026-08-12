# A JDK-generated EC server identity was rejected by the TLS stack — every EC handshake hung

**Status:** FIXED (2026-08-12) in `native-builtins/src/t27_tls.rs`. Found while
working [investigate-batch-08.md](investigate-batch-08.md).

## Symptom

`io.netty.handler.codec.http2.Http2MultiplexTransportTest` reported one failing
test on CratonVM that passes on stock HotSpot:

```
testFireChannelReadAfterHandshakeSuccess_JDK()
    java.util.concurrent.TimeoutException: timed out after 5000 milliseconds
```

The fixture's `@Timeout(5000)` made this look like the familiar throughput gap.
It is not. Re-running the class with JUnit's timeouts disabled
(`-Djunit.jupiter.execution.timeout.mode=disabled`) settles it:

| | wall clock | result |
|---|---|---|
| HotSpot JDK 25 | 2.6 s | whole class completes |
| CratonVM (dev) | 400 s (harness cap) | never completes |

A **hang**, not slowness.

## Root cause

`--stack-dump-on-timeout=60` puts the main thread at
`Http2MultiplexTransportTest.testFireChannelReadAfterHandshakeSuccess` bci 306 —
`CountDownLatch.await()` — with the netty event loop parked in `select()`,
i.e. waiting for bytes that will never arrive. Two lines in the same log say why:

```
WARN i.n.h.s.ApplicationProtocolNegotiationHandler - Failed to select the application-level protocol:
  io.netty.handler.codec.DecoderException: java.io.IOException:
    ServerConfig with_single_cert failed: unexpected error:
      failed to parse private key as RSA, ECDSA, or EdDSA
        at io.netty.handler.ssl.JdkSslEngine.wrap(JdkSslEngine.java:79)
```

The server's TLS engine could not be built at all, so ALPN never selected `h2`,
so the server never spoke HTTP/2, so the client's latch was never counted down.

The rejected key is the one the test generates with netty's `CertificateBuilder`
— i.e. `java.security.KeyPairGenerator("EC")`. Dumped with
`CRATONVM_DBG_TLS_HS=1`, it is 67 bytes:

```
3041                                    SEQUENCE (PKCS#8 PrivateKeyInfo)
  020100                                version 0
  3013 06072a8648ce3d0201               AlgorithmIdentifier: id-ecPublicKey
       06082a8648ce3d030107                                  prime256v1
  0427                                  privateKey OCTET STRING
    3025                                  SEC1 ECPrivateKey
      020101                                version 1
      0420 <32-byte scalar>                privateKey
                                          -- no parameters [0]
                                          -- no publicKey  [1]
```

`ring` — the crypto backend under rustls here, including the vendored
`rustls-cbc` — can only build an `EcdsaKeyPair` from a PKCS#8 document that
**does** carry `publicKey [1]`. The SEC1 branch is no escape hatch:
`EcdsaSigningKey::convert_sec1_to_pkcs8` re-wraps and calls the same
`from_pkcs8`. rustls collapses every refusal into the one generic message
above, which reads like a corrupt key and is not.

**The key is not the bug.** A probe run on both VMs shows HotSpot's SunEC emits
a byte-identical 67-byte P-256 encoding, also with no `publicKey [1]`:

| query | HotSpot JDK 25 | CratonVM |
|---|---|---|
| `KeyPairGenerator("EC")` P-256 `getEncoded().length` | 67 | 67 |
| inner SEC1 `parameters[0]` / `publicKey[1]` | absent / absent | absent / absent |
| P-384 `getEncoded().length` | 80 | 80 |

So this is purely a consumer-side defect: **every EC identity the JDK's own
`KeyPairGenerator` produces was unusable by CratonVM's TLS stack.** RSA was
unaffected (its PKCS#8 needs no such fixup), which is why the gap went unnoticed
— most test fixtures and keystores in the corpus are RSA.

Isolated to a single variable in `t27_tls::ec_pkcs8_v1_identity_tests`: take one
openssl-generated P-256 key that ring accepts, strip only the optional fields
from its inner SEC1, and ring rejects it. Nothing else changes.

## Fix

`ec_pkcs8_splice_public_key(key_der, leaf_cert_der)` recovers the missing public
key from the **leaf certificate's `SubjectPublicKeyInfo`**, which by definition
holds the public key for this identity, and re-emits the PKCS#8 with
`publicKey [1]` spliced into the inner SEC1.

Taking the point from the certificate rather than deriving it (`d·G`) matters:
the in-tree `crypto_impl` EC core is P-256 only, so a derivation would have
fixed one curve. Reading it out of the cert is curve-agnostic and needs no EC
arithmetic — P-384 is covered by the same code path and is pinned by a test.

It is also self-checking. `ring`'s `from_pkcs8` recomputes the public key from
the private scalar and rejects the document if the two disagree, so splicing a
**mismatched** certificate fails closed exactly as before rather than producing
an identity that signs with the wrong key. That property is pinned by
`splice_declines_when_the_certificate_cannot_lend_a_matching_point`, which
splices a P-384 certificate into a P-256 key and asserts ring still refuses.

Every failure path returns `None` — "use the key unchanged" — so the repair can
only make more identities usable, never fewer. It is applied at all seven places
a key meets its chain: both server-config builders, the SNI cert resolver, and
the four mTLS client-identity builders (a JDK-generated EC *client* certificate
had the same problem).

## Result

`Http2MultiplexTransportTest` now matches HotSpot exactly and finishes in 5.5 s
instead of hanging:

```
                          HotSpot: found=11 started=9 ok=6 failed=0 aborted=3 skipped=2
CratonVM before the fix:           found=11 started=9 ok=5 failed=1 aborted=3 skipped=2
CratonVM after  the fix:           found=11 started=9 ok=6 failed=0 aborted=3 skipped=2
```

## Coverage

`t27_tls::ec_pkcs8_v1_identity_tests` — six tests over frozen P-256/P-384
key+certificate vectors (generated once with openssl and embedded as hex, so the
tests need no crypto dependency and run on every platform, including the Windows
build where the `openssl` crate is not a dependency).

Each round trip asserts **both** directions: that the stripped JDK shape is
genuinely rejected by ring without the splice, and that it is accepted with it.
The negative half is what keeps the test able to fail — delete the splice and
the module goes red rather than silently passing. The remaining tests pin that
the repair declines a key that already has a public key, a non-EC key, a
certificate that cannot lend a matching point, and truncated/garbage DER on
both inputs without panicking.

## Repro (Linux host)

```bash
cd /data/cratonvm/apps/netty-suite-runner
CRATONVM_DBG_TLS_HS=1 <cv-bin> --java-home "$JAVA_HOME" --Xmx 1500m \
    -Djunit.jupiter.execution.timeout.mode=disabled \
    @common.args -Dcraton.batch=1 CratonRunner \
    io.netty.handler.codec.http2.Http2MultiplexTransportTest
```
