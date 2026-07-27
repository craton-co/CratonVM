# ServerHttpsRequestIntegrationTests: RUNTIME_TLS_IDENTITY singleton clobbered by a malformed key [FIXED]

Status: ✅ **FIXED 2026-07-07** (branch `fix/tls-identity-singleton-clobber-20260707`).
The doc's documented defect — a malformed private key clobbering the
process-global `RUNTIME_TLS_IDENTITY` and breaking every subsequent handshake —
is resolved, along with the deeper root cause of *why* the key was malformed and
two further encoding bugs in the same cert/key setup chain. **FOUR distinct
CratonVM bugs** were found and fixed here (each verified against HotSpot in
isolation); the server rustls `ServerConfig` now builds successfully where it
previously failed at `failed to parse private key`.

The `ServerHttpsRequestIntegrationTests` test itself is still red, but now on a
**fifth, separate** issue — the Reactor-Netty SSLEngine handshake stalls at
`BUFFER_UNDERFLOW` in TLS record data-flow, a different subsystem this doc never
covered — tracked in
[`../known-issues/reactive-netty-https-sslengine-handshake-underflow.md`](../known-issues/reactive-netty-https-sslengine-handshake-underflow.md).

## The four fixed bugs (in the order the handshake exposed them)

1. **RSA `KeyPairGenerator` produced non-CRT keys with a malformed 572-byte
   `getEncoded()`** — the doc's "malformed key" mystery (open question #2 below).
   CratonVM's default fast RSA keygen dropped the primes `p`/`q` and rebuilt the
   private key via a 2-arg `RSAPrivateKeySpec(n, d)` → a non-CRT
   `sun.security.rsa.RSAPrivateKeyImpl` whose PKCS#8 has only modulus + private
   exponent (`e` and all five CRT params encoded as INTEGER 0), where HotSpot
   emits a complete 1216-byte `RSAPrivateCrtKeyImpl`. rustls rejects the former
   (`failed to parse private key as RSA, ECDSA, or EdDSA`). Fixed by carrying the
   CRT parameters on `crypto_impl::RsaPrivateKey` and building the real key via
   `RSAPrivateCrtKeySpec` (`../../../native-builtins/src/crypto_impl.rs`,
   `jca/key_factory.rs`). Verified: `getEncoded()` 572 → 1217 bytes,
   `RSAPrivateCrtKey`, sign/verify round-trip.
2. **`RUNTIME_TLS_IDENTITY` clobber defense** (this doc's headline, fix candidate
   (b)): `install_identity_from_der` now validates a candidate identity actually
   builds a rustls `ServerConfig` before overwriting an already-installed working
   one (`../../../native-builtins/src/t27_tls.rs`), so an unrelated/malformed
   `KeyStore.setKeyEntry` can no longer silently break a server that already has a
   valid identity.
3. **`CertificateFactory.generateCertificate` returned empty `getEncoded()` on
   PEM streams** — the native read the raw stream bytes as if always-DER; a
   PEM-armored stream produced a cert with empty DER. Fixed with PEM-armor
   detection + base64-decode (`pem_block_to_der`, `../../../native-builtins/src/lib.rs`).
4. **`generateCertificate`/`generateCertificates` only read a
   `ByteArrayInputStream`'s field-0 buffer** — any other `InputStream` subtype
   (Netty's cert stream) read zero bytes → empty-cert stub → rustls
   `invalid peer certificate: BadEncoding`. Fixed by reading via the generic
   `p59_read_input_stream_fully` helper (`../../../native-builtins/src/phases_late.rs`).
   Verified: Netty `SelfSignedCertificate().cert().getEncoded()` 0 → 686 bytes,
   real `X509CertImpl`.

Historical context: split out of the retired `http-server-cluster-residuals.md`
(full history: `docs/internal/fixed-suite-bugs/http-server-cluster-fixes-FIXED.md`,
which fixed 4 other real bugs in this same test's dependency chain).

## Symptom

`org.springframework.http.server.reactive.ServerHttpsRequestIntegrationTests
::checkUri()` fails:

```
org.springframework.web.client.ResourceAccessException: I/O error on POST
request for "https://localhost:PORT/foo": TLS handshake failed: unexpected EOF
```

## Prior hypothesis — REFUTED

An earlier session traced this to `do_wrap`/`do_unwrap`
(`../../../native-builtins/src/t27_tls.rs`) and found the server engine's very
first `do_wrap()` call short-circuited immediately because
`s.closed_outbound` was already `true` — Java (Netty's `SslHandler`) was
calling `closeOutbound()` right after the first `unwrap()`, suggesting
Netty was reacting to something in the `SSLEngineResult` it got back from
that `unwrap()` call. That session could not capture the actual
`SSLEngineResult` values despite instrumenting every explicit return path
in `do_unwrap`, and left this as the next step.

**This hypothesis is refuted.** A later session added a bulletproof
`__ScopeExit` RAII guard (fires on ANY exit path — return, `?`, or panic)
at the top of `do_unwrap` and found the entry immediately followed by a
`closeOutbound()` call with **no exit print firing at all** — meaning
`do_unwrap`/`do_wrap`'s TLS record-processing logic was never actually
reached to produce a semantically-meaningful result to misinterpret.

## Actual root cause

`engine_begin()` (`../../../native-builtins/src/t27_tls.rs`) fails outright, on the
very first `do_unwrap`/`do_wrap` call, with:

```
ServerConfig with_single_cert failed: unexpected error: failed to parse
private key as RSA, ECDSA, or EdDSA
```

Netty's `closeOutbound()` is simply its normal reaction to a broken
`SSLEngine`, not a misread of a wrap/unwrap semantic result.

Traced further via call-site-tagged instrumentation
(`CRATONVM_DBG_TLS_HS=1`): `engine_set_key_entry`
(`../../../native-builtins/src/keystore.rs`) calls `t27_tls::install_identity_from_der`
**twice** during this test's setup, both times from the direct-API path
(not the `engineLoad` byte-stream path):

1. 1st call: `key_len=1217` bytes — verified via a manual ASN.1 walk to be
   a complete, well-formed 2048-bit RSA PKCS8 private key (modulus 257
   bytes ≈ 2048 bits; the full byte count is consistent with a complete
   RSA CRT-form private key: n, e, d, p, q, dp, dq, qinv).
2. 2nd call: `key_len=572` bytes — the DER header is ALSO PKCS8-shaped
   (same `rsaEncryption` OID, confirmed via `sniff_private_key_pem_header`,
   see below), but the total byte count is far too small to contain a
   complete 2048-bit RSA private key with CRT parameters (the modulus
   alone would already consume nearly half the remaining bytes after
   the outer wrapper) — genuinely malformed/truncated content, not just a
   smaller (e.g. 1024-bit) key.

`install_identity_from_der` writes into a single process-wide
`RUNTIME_TLS_IDENTITY` singleton (`static RUNTIME_TLS_IDENTITY:
OnceLock<Mutex<Option<RuntimeTlsIdentity>>>`) with last-write-wins
semantics and no validation. The 2nd (bad) call silently overwrites the
1st (good) identity before `engine_begin` — which reads the singleton
lazily, on the first TLS record — ever gets to use it.

A follow-up check (independent verification pass) additionally found:
`ctx_identity()` (which should populate a per-`SSLContext`
`identity_override` that would bypass the global singleton entirely)
returns no matching stored identity for this test's server `SSLContext`,
so `engine_begin` falls through to `default_engine_server_config()` →
`runtime_tls_identity()` → the clobbered global. This is *why* the
process-global path is even reached for this test.

## Ruled out

- **Wrong PEM header / wrong key-type labeling** (the first fix attempt):
  `sniff_private_key_pem_header`, a new ASN.1-shape-based PKCS8/PKCS1/SEC1
  auto-detector added to replace `install_identity_from_der`'s prior
  hardcoded "always PKCS8" assumption, correctly identifies BOTH keys'
  headers as PKCS8 — the header is genuinely right, the *content* is what's
  wrong for the 2nd key. Kept as an independent robustness improvement
  (any genuinely PKCS1/SEC1-encoded key handed to this function elsewhere
  would previously have been silently mislabeled and rejected by rustls;
  now auto-detected) but did not fix this bug.
- **`do_wrap`/`do_unwrap` SSLEngineResult semantics** — see "Prior
  hypothesis" above.

## Not yet determined (next steps)

1. **Why does the 2nd `setKeyEntry` call happen at all?** Both calls come
   from the same native (`engine_set_key_entry`), so this isn't a
   difference in parsing code — it's two distinct Java-level
   `KeyStore.setKeyEntry(...)` invocations during test setup. Determine
   what Java code performs the second call and what `PrivateKey`
   object/algorithm/purpose it's installing (e.g. is it actually meant to
   be a client-side/truststore-scoped key that should never have touched
   the server's identity singleton at all?).
2. **Why is the 2nd key's content malformed?** Once the caller/purpose is
   known, trace whether `key.getEncoded()` genuinely returns truncated
   bytes for that specific key object (a bug in whatever produces it —
   possibly a synthetic or partially-real key object with an incomplete
   `getEncoded()`), or whether `engine_set_key_entry`'s own byte-array
   read loop (`ctx.array_length`/`ctx.get_array_element`, `keystore.rs`)
   has a bug specific to this second array instance.
3. **Design fix candidates**, once (1)/(2) are known: either (a) make
   `RUNTIME_TLS_IDENTITY` properly scoped per-`KeyStore`/`SSLContext`
   instead of one process-wide global, so an unrelated `setKeyEntry` call
   can't clobber the real server identity, and/or (b) validate a new
   identity (e.g. attempt to actually construct a rustls `ServerConfig`
   from it) before overwriting a previously-working one, falling back to
   keeping the old identity and logging a warning on failure rather than
   silently breaking every subsequent handshake.

A standalone, single-threaded JDK 25 baseline probe (`SSLEngineProbe.java`,
not committed — scratch repro) confirms what a healthy handshake's first
server `unwrap()` legitimately returns (`status=OK,
handshakeStatus=NEED_TASK, bytesConsumed=422, bytesProduced=0`) — useful
ground truth for whoever verifies the eventual fix, preserved here since
it's now moot for the actual bug (which never reaches real TLS record
processing at all).

## Reproduction

```bash
CP="$(cat /data/data/spring-framework-shared/spring-web/build/cratonvm-testcp.txt)"
CRATONVM_DBG_TLS_HS=1 timeout 60 ./target/release/cratonvm \
  --java-home /data/data/jdk25-real \
  -cp "/data/data/spring-suite-runner-shared:$CP" \
  KRun org.springframework.http.server.reactive.ServerHttpsRequestIntegrationTests
# Reproduces 100% of runs (not intermittent). Look for:
#   "[dbg-tls-hs] install_identity_from_der CALLER=engine_set_key_entry(direct-API) key_len=..."
#   (fires twice, second one much smaller)
#   "[dbg-tls-hs] ... RETURN(engine_begin ERROR) err=ServerConfig with_single_cert failed: ..."
```

BouncyCastle must be on the classpath (`bcpkix-jdk18on`/`bcprov-jdk18on`
via `spring-web`'s `testFixturesImplementation`) — confirm present in
`$CP` if this stops reproducing.
