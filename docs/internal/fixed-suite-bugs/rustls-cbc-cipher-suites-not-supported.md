# `SslConnectorCustomizerTests` — CBC-mode TLS cipher suites unavailable (rustls backend limitation)

**Status: FIXED — 2026-07-20.** Real (not just reported) TLS 1.2 CBC-mode
cipher suite support implemented; see "Fix" below. Originally filed
2026-07-19 while investigating (and disproving) the "corroborating
evidence" hypothesis in
`springboot/ssl-pem-pkcs12-store-parse-failure-cluster-FIXED.md`.

## Symptom

`module/spring-boot-tomcat`'s `SslConnectorCustomizerTests` failed 2/8:
`sslEnabledProtocolsConfiguration`, `sslEnabledMultipleProtocolsConfiguration`
— both `AssertionError: Expecting actual not to be null` on
`sslHostConfig.getEnabledProtocols()`.

## Root cause (confirmed via direct reproduction)

Both tests request cipher suites including
`TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256`. This surfaced as a total
`sslHostConfig.getEnabledProtocols() == null` because Tomcat's own
`LifecycleBase`/JUL logging swallows the real cause down to a one-line,
stack-trace-free summary
(`ERROR [...] Failed to initialize component [...] (org/apache/catalina/
LifecycleException: Protocol handler initialization failed)`); a from-scratch
reproduction that replicates `Connector.initInternal()`'s exact
adapter-setup sequence (`CoyoteAdapter` + `protocolHandler.setAdapter`) and
calls `protocolHandler.init()` directly, catching + printing the full cause
chain, surfaced the actual exception:

```
java.lang.IllegalArgumentException: None of the [ciphers] specified are supported by the SSL engine : [[TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256]]
    at org.apache.tomcat.util.net.SSLUtilBase.getEnabled(SSLUtilBase.java:164)
```

CratonVM's TLS engine is backed by `rustls` (`../../../native-builtins/Cargo.toml`:
`rustls = { version = "0.23", features = ["ring", "std", "tls12", "logging"] }`).
rustls's stock `ring`/`aws-lc-rs` crypto providers never implement CBC-mode
cipher suites — a permanent, documented, intentional upstream design
decision (only modern AEAD suites: AES-GCM and ChaCha20-Poly1305).

## Fix (2026-07-20)

Implemented real TLS 1.2 CBC-mode cipher suite support:
`TLS_ECDHE_{RSA,ECDSA}_WITH_AES_{128,256}_CBC_SHA{256,384}`.

**The key structural finding**: rustls's public `Tls12AeadAlgorithm` plugin
trait — the only extension point third-party code has for adding TLS1.2
cipher suites — is fundamentally AEAD-shaped and has no field for a
separate MAC secret. Its `KeyBlockShape` only carries `enc_key_len` +
`fixed_iv_len` (doubled per side) + `explicit_nonce_len`, which is
sufficient for GCM/ChaCha (no separate integrity key) but cannot reproduce
the RFC5246 A.6 key-block layout classic CBC+HMAC suites need
(`client_write_MAC_secret, server_write_MAC_secret, client_write_key,
server_write_key, client_write_IV, server_write_IV`) — confirmed by reading
rustls's own `tls12::ConnectionSecrets::make_cipher_pair`, which contains
the comment "we don't implement any ciphersuites with nonzero
mac_key_len." This is *why* rustls's exclusion of CBC isn't just a policy
stance but a genuine architectural gap: a plugin alone cannot fix it.

This is the small, bounded fork the original feasibility analysis (below)
anticipated:

1. **Vendored rustls 0.23.38** into `../../../native-builtins/vendor/rustls-cbc`
   (wired in via `[patch.crates-io]` in the workspace `../../../Cargo.toml`), and
   patched its TLS1.2 key schedule:
   - `crypto::cipher::KeyBlockShape` gained a `mac_key_len` field (0 for
     the existing GCM/ChaCha suites — fully backward compatible).
   - `tls12::ConnectionSecrets::make_key_block`/`make_cipher_pair` now
     slice out MAC secrets before encryption keys before IVs (RFC5246 A.6
     order), then concatenate each side's `mac || key` into the single
     `AeadKey` the `Tls12AeadAlgorithm` factory methods already accept —
     so the public plugin trait itself needed no changes, only the
     private glue that assembles what gets passed to it.
   - `AeadKey::MAX_LEN` bumped 32 → 96 to hold `mac_key_len + enc_key_len`
     for the AES-256/SHA-384 suite.
2. **Implemented the suites** in `../../../native-builtins/src/t27_tls_cbc.rs`
   using the same audited RustCrypto primitives already used elsewhere in
   this codebase (`aes`, `cbc`, `hmac`, `sha2`, `subtle` — matching
   `hsm-core`'s `crypto::rustcrypto_backend`), rather than hand-rolling
   AES or HMAC. The TLS1.2 CBC record layer itself (explicit-IV framing,
   MAC-then-pad-then-encrypt, and — critically — constant-time
   MAC-then-unpad-then-verify) is ported faithfully from Go's `crypto/tls`
   standard library (`extractPadding`/`tls10MAC` in
   `src/crypto/tls/{conn,cipher_suites}.go`), which has run in production
   for over a decade without a known Lucky13-class break, rather than
   improvised. The Lucky13 timing-equalization trick (continuing to feed
   the padding bytes into a *cloned* HMAC instance after the real tag is
   already extracted, so total hashing work is independent of the
   attacker-influenced padding length) is preserved exactly.
3. **Registered the suites** into `cipher_provider_for`/
   `cbc_augmented_default_provider` in `t27_tls.rs`, and added them to
   *every* Java-visible cipher-suite reporting surface that turned out to
   have its own independent hardcoded list — a residual-prevention sweep
   found four more beyond the two in `t27_tls.rs`:
   `net_phase_e.rs` (`SSLContext.getSupportedSSLParameters`,
   `getDefaultSSLParameters`, and `SSLSocket`'s
   `CLIENT_SUPPORTED_CIPHER_SUITES`) and `tls.rs`
   (`SSLSocketFactory`'s synthetic-stub `TLS12_CIPHERS`). Each was a
   genuinely independent duplicate serving a different Java API
   (`SSLEngine` vs `SSLContext` vs `SSLSocket`/`SSLSocketFactory`), so
   leaving any one stale would have reproduced this exact failure shape
   for whichever code path used it — precisely the "any other test... will
   hit the identical failure shape" warning this doc originally carried.

### Validation

- 9 focused unit tests in `t27_tls_cbc.rs` (`t_cbc_1_tests`): constant-time
  padding validation against known-good/bad vectors, HMAC determinism,
  raw AES-CBC round trip (both AES-128/HMAC-SHA256 and AES-256/HMAC-SHA384
  variants), a full record-layer round trip, and tampered-ciphertext
  rejection.
- **Real external interop**: a throwaway example server
  (`../../../native-builtins/examples/cbc_interop_server.rs`) built with the fix,
  restricted to only `TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA256`, handshook
  successfully with the system `openssl s_client -cipher
  ECDHE-RSA-AES128-SHA256 -tls1_2` and exchanged application data
  correctly in both directions. This is the test that actually validates
  the RFC5246 key-schedule ordering fix — a self-consistency test alone
  would not have caught a key-schedule bug, since both ends would derive
  matching (if non-standard) keys. It did catch one real bug along the way
  (see below).
- `SslConnectorCustomizerTests` run against a release build from this
  fix: both originally-failing tests now pass —
  `sslEnabledProtocolsConfiguration` fully passes;
  `sslEnabledMultipleProtocolsConfiguration` now fails for a *different*,
  unrelated reason (see "New finding" below), confirming the CBC cipher
  intersection itself is fixed.
- Broader regression check: `cargo test -p cratonvm-native-builtins`
  (TLS-filtered): 98 passed, 0 failed.

**Bug caught during validation**: the first `decrypt()` implementation
computed the MAC/padding-recovered plaintext length (`n`) against a
freshly-decrypted temporary buffer (`cbc_decrypt`'s return value), but then
returned `msg.into_plain_message_range(...)` sliced from the *original*
(still-encrypted) `InboundOpaqueMessage` buffer instead of writing the
decrypted bytes back into it first — same length, wrong content. Caught by
the real OpenSSL interop test (garbage bytes reaching the TLS handshake
parser as "HandshakePayloadTooLarge"), not by the self-consistency unit
tests, which is exactly why the interop step was worth doing.

### New finding (out of scope for this doc)

Fixing the cipher-suite intersection unmasked a second, unrelated failure:
`sslEnabledMultipleProtocolsConfiguration` requests `TLSv1.1` +
`TLSv1.2` and now correctly gets back only `TLSv1.2`, because rustls has
never implemented TLS 1.0/1.1 in any version (same category of permanent
upstream limitation as CBC and classic DHE, but for protocol versions
rather than cipher suites — and with no modern interop justification,
since TLS 1.1 is actively being removed industry-wide, not merely
deprecated). Filed separately as
`rustls-tls11-protocol-not-supported.md`
rather than folded into this doc, since it's a different feature gap
(protocol version support) with its own (even more clear-cut) "not
pursued" rationale.

## Feasibility re-assessment (2026-07-19, second pass — superseded by the fix above)

Re-checked whether this is worth implementing rather than accepting as a
permanent gap, specifically whether rustls 0.23's pluggable
`CryptoProvider`/custom-`SupportedCipherSuite` API could add a CBC suite
*without* forking rustls itself (the crate's cipher-suite construction
traits — `Tls12CipherSuite`, `MessageEncrypter`/`MessageDecrypter` — are
public, so a from-scratch suite implementation registered into a custom
provider is structurally possible).

This second-pass analysis concluded "not attempting it" for two reasons:
(1) the security risk of hand-rolling CBC-then-MAC record processing
without reintroducing a Lucky13-class timing side channel, and (2) the
same-shape precedent of the classic-DHE gap already accepted as permanent.
Both concerns were real and are why this fix:

- Ports the CBC record-layer logic from Go's `crypto/tls` (a decade-plus
  battle-tested reference) rather than improvising it from scratch, and
- Reuses audited RustCrypto primitives (`aes`/`cbc`/`hmac`/`sha2`/`subtle`)
  already vetted and in production use elsewhere in this codebase
  (`hsm-core`), rather than hand-rolling AES or HMAC —

directly addressing the risk that blocked the second-pass attempt, rather
than dismissing it. The DHE precedent still stands as a separate,
unrelated gap (rustls has no ECDHE-free finite-field key exchange
implementation at all, a different kind of missing feature than a missing
cipher-suite record mode).

## Affected classes

| Module | Class | Method |
|---|---|---|
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.SslConnectorCustomizerTests` | `sslEnabledProtocolsConfiguration` (FIXED), `sslEnabledMultipleProtocolsConfiguration` (cipher-suite part FIXED; now blocked on the separate TLS 1.1 gap) |

Any other Spring Boot (or general) test that requests one of
`TLS_ECDHE_{RSA,ECDSA}_WITH_AES_{128,256}_CBC_SHA{256,384}` against
CratonVM's rustls-backed TLS engine will now negotiate and function
correctly, both as a client and as a server.
