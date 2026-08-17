# netty `JdkSslEngineTest` — engine-level gaps, per cause (FIXED)

**Status: RESOLVED 2026-08-15.** This class now matches the HotSpot 25 oracle
exactly — 755 ok, **0 failed**, 66 aborted — and is retired here from
`docs/known-issues/netty/`. Successor to R6 of the retired
`tls-batch10-residuals-20260813` write-up.

| | tests | ok | failed | aborted | wall |
|---|---|---|---|---|---|
| HotSpot 25 | 821 | 755 | **0** | 66 | 98 s |
| CratonVM (`fcb509bda`, 2026-08-13) | 821 | 398 | 357 | 66 | 630 s |
| CratonVM (V1 landed, 2026-08-13) | 821 | 448 | 307 | 66 | 516 s |
| CratonVM (`2cb9217e5` = dev, 2026-08-15) | 821 | — | — | — | never finished (killed at 2400 s) |
| CratonVM (`2c5160834`, 2026-08-15) | 821 | 707 | 48 | 66 | 342 s |
| CratonVM (`e5f4597e6`, 2026-08-15) | 821 | **755** | **0** | 66 | **315 s** |

The 66 aborts are the same 66 HotSpot aborts (Conscrypt is not on the
classpath, and `testMasterKeyLogging`'s assumption is false on both). The
wall-clock ratio is **3.2x**, down from 6.2x, and is now a throughput question
rather than a correctness one — the earlier rounds' clock came off by fixing
tests that failed by *waiting*, and there are none of those left.

## The last four causes and what each turned out to be

Everything before these is in the commit log (`2c5160834` and its ancestors);
this section is only the residual the page was left open on.

### `testRSASSAPSS` (12) — PSS parameters are not derivable from the OID

`RSASSA-PSS` is the one signature OID that does not name its own digest. The
digest, the MGF1 digest and the **salt length** live in the
`RSASSA-PSS-params` AlgorithmIdentifier, and RFC 4055 §3.1 defaults the salt
length to **20 whatever the digest is** — it is not hLen. The previous
dispatch tried SHA-256/384/512 at salt length hLen and could therefore never
verify netty's fixtures, which are SHA-256 / MGF1-SHA-256 / **salt 20**:

```
rsapss-ca-cert.cert            Hash sha256  Mask mgf1 sha256  Salt 0x14 (default)
rsaValidation-user-certs.p12   ditto
rsaValidations-server-keystore.p12  ditto
```

Every attempt failed, `validate_chain` answered `BadSignature`, the
`TrustManager` reported a rejected chain and the peer got `certificate_unknown`.

`ParsedCert` now keeps `signature_algorithm_params`; `parse_rsa_pss_params`
reads them (refusing an unimplemented digest, a non-MGF1 mask and a
non-default trailer field rather than defaulting); `rsa_verify_pss_ex` takes
the MGF digest and salt length explicitly. Pinned by
`validate_chain_rsa_pss_uses_the_params_salt_length_not_hlen`, which checks
BOTH directions so neither assumption can come back.

### `testMutualAuthDiffCerts` (12) — the page's own hypothesis was wrong

The open page said "same root cause: `test.crt`/`test2.crt` are PSS-signed".
They are not — both are `sha256WithRSAEncryption`. **`test2.crt` expired on
16 November 2014**, it is the server's only trust anchor in this test, and the
client presents it as its own identity.

RFC 5280 §6.1 validates the certificates ON the path against the anchor; the
anchor is an input to that algorithm, not a member of it, so its validity
period is never checked. JSSE agrees by construction — `PKIXValidator` strips
a trailing trusted certificate before handing the remainder to
`CertPathValidator`, so a one-element chain that IS an anchor validates as the
empty path. `validate_ordered_chain`'s clock check now skips a presented
certificate that is a stored anchor, compared by full DER: a same-subject
impostor is still expired.

### `testClientHostnameValidationFail` (12) — two halves

1. **Nobody identified.** `jsse_owns_endpoint_identification` answered "the
   application owns it" for any `X509ExtendedTrustManager`, and
   `SslContextBuilder.trustManager(File)` produces
   `sun.security.ssl.X509TrustManagerImpl` — JSSE's own, whose
   `checkServerTrusted` is served here by a native shim that never sees the
   `SSLEngine` and so cannot read
   `SSLParameters.getEndpointIdentificationAlgorithm()`. A predicate has to
   mirror the dispatch it guards.
2. **The check was too late.** It ran after the handshake, by which time the
   client's `Finished` had gone out and the server had completed — so the
   test's server handler recorded `IllegalStateException("handshake complete.
   expected failure")` even once the client did reject the certificate. It is
   a pure comparison of the presented chain against the host dialled, so it
   now runs inside the rustls verifier, gated on the same predicate, with the
   post-handshake copy kept for the paths that never build a config there.

### `doHandshakeVerifyReusedAndClose` (12) — resumption needs identical `Arc`s

rustls refuses to resume unless the `ServerCertVerifier` **and** the
`ResolvesClientCert` are the same `Arc` as when the session was stored
(`Tls13ClientSessionValue::compatible_config`, pointer identity — resuming
across a different verifier would inherit a trust decision it never made).
`engine_begin` builds a fresh config per engine, so the client took its ticket
out of the store and then never offered it, and every handshake was `Full`.

The signature in a trace, once the stores are decorated (they are, under
`CRATONVM_DBG_TLS_AUTH=1`): `insert_tls13_ticket` then `take_tls13_ticket ->
true`, and then **no `server store: take` at all** — the server only consults
`session_storage` when the ClientHello actually carries a
`preshared_key_offer`.

Configs are now cached per `(SSLContext, engine shape)`, shape being
everything that genuinely varies (ciphers, protocols, ALPN, endpoint identity,
client-auth mode) — sharing across shapes would let one engine's
`setEnabledCipherSuites` govern the next engine's handshake. The
`RecordingClientCertResolver` is installed only for a client that has an
identity to present; without one there is nothing to record, and
`client_cert_presented == None` already means "nothing sent".
`SSLEngine.getSession()` then answers with the PREVIOUS session object for a
handshake rustls reports as `Resumed`, which is what makes the values a caller
put on it readable again.

## One residual, and it is not a correctness one

`testMutualAuthSameCertChain` takes 100–180 s for its 12 parameterisations
(8–15 s each) against a per-test `@Timeout` of 30 s. On a loaded host one
parameterisation can tip over it: seen once in a full-class run, and 12/12 in
four consecutive isolated runs (base and fixed builds interleaved). It is
slowness near a threshold, not a defect — but it is why a single
`TimeoutException` from that method in a busy run should be re-run before it
is believed.

## Repro

```bash
cd /data/cratonvm/apps/netty-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv-bin> --java-home "$JAVA_HOME" --Xmx 1500m \
    @common.args -Dcraton.batch=1 CratonRunner io.netty.handler.ssl.JdkSslEngineTest
```

`common.args` MUST carry `netty-tcnative-boringssl-static-<ver>-<os>.jar` in
place of the dynamic `netty-tcnative` the Maven reactor resolves — see the
sibling `openssl-key-material-and-engine-residuals` page for why, and for what
a run without it silently measures. Give it at least 600 s.
