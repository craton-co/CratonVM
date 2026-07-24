# `NettyReactiveWebServerFactoryTests.sslWithPemCertificates` — rustls fatal alerts during mutual-TLS handshake

**Status: FIXED 2026-07-20.**

This doc consolidates two separate investigation sessions that ran
concurrently on 2026-07-20 in separate worktrees and reached compatible
conclusions: an earlier session (folded in below, "prior session") narrowed
the root cause to the exact alias-ambiguity mechanism this fix addresses,
found and fixed a genuinely separate `KeyStore.getKey()`/`Signature.sign()`
bug along the way, attempted a fix using `JavaKeyManagerResolver`, and
reverted it after it regressed `sslNeedsClientAuthenticationSucceedsWithClientCertificate`.
This session's fix (below) reaches the same diagnosis via a different
mechanism (a static, precomputed alias-selection lookup rather than a live
`JavaKeyManagerResolver::resolve()` call) that does not hit that
regression — verified directly against that exact test method, see
Verification.

## Symptom (as originally filed)

`sslWithPemCertificates()` configures the server with client-auth `NEED`, a
PEM certificate+key (in-memory `PemSslStoreBundle`), and a PEM trust
certificate. The client presents a PKCS12 identity
(`buildTrustAllSslWithClientKeyConnector("test.p12", "secret")`). The
handshake failed with either:

```
rustls process_new_packets: received fatal alert: DecryptError
```

(full 36-method class run), or, when the test method was run in isolation:

```
rustls process_new_packets: received fatal alert: CertificateRequired
```

Both alerts turned out to be two faces of the same root cause, not two
separate bugs and not cross-test pollution.

## Root cause

`SSLContext.init(KeyManager[], TrustManager[], SecureRandom)`'s native
handler (`net_phase_e.rs`/`phases_late.rs`) attached a per-`SSLContext` mTLS
identity by draining a **thread-local "most recently staged" slot**
(`PENDING_KM_IDENTITY`, set by `KeyManagerFactory.init` and consumed by
"whichever `SSLContext.init` call runs next on this thread"). That
assumption holds when a `KeyManagerFactory.init` is immediately followed by
its own `SSLContext.init` — but Reactor Netty builds **several**
`SSLContext`s from **several** `KeyManagerFactory`s (protocol/cipher-support
probe contexts, an internal Netty-built in-memory keystore, etc.) before
either the real client or real server context is actually used. An
intervening, entirely unrelated `SSLContext.init` call could drain the
thread-local before the context that actually needed it ever consumed it —
silently leaving that context, and every engine created from it, with
**no** identity at all.

- In isolation, this left the **client's** context with no identity: it
  presented an empty Certificate message, the server (requiring client auth)
  rejected with `certificate_required`.
- In the full class, timing/ordering differences left a **different**
  context under-resourced, eventually surfacing as `DecryptError`.

Fixing the "which context drains the slot" ordering (task 1 below) then
exposed a **second**, independent bug: the fallback logic that resolves an
identity directly from a keystore id picked the **first entry in file
order** — but `test.p12` (this test's own PKCS12 fixture) intentionally
carries two client identities, `spring-boot` and `test-alias`, side by side,
only one of which (`test-alias`) the server's truststore accepts (see
`x509_manager.rs`'s `java_hashmap_iteration_order` doc comment, which
already documented this exact fixture as the reason `chooseClientAlias` has
to replicate real JDK's `HashMap`-bucket iteration order rather than
"keystore file order"). The naive first-entry fallback silently picked
`spring-boot`, which the server didn't trust in the way this test needs,
producing `InvalidCertificate(BadSignature)` server-side and `DecryptError`
client-side.

## Fix

Two changes, both in `native-builtins/src/`:

1. **Resolve identity directly from the actual `KeyManager[]` a given
   `SSLContext.init` call received**, instead of relying solely on the
   thread-local ordering assumption. `x509_manager::
   resolved_identity_pem_for_key_manager_array` traces each `KeyManager`
   object's `km_id` (already stamped by `KeyManagerFactory.getKeyManagers`)
   back to its `KeyManagerState`, immune to any number of unrelated,
   interleaved `SSLContext.init` calls stealing the thread-local first. The
   thread-local fallback is preserved for the case this can't resolve an id
   (e.g. a test wrapper like Tomcat's `TrackingKeyManager` that doesn't
   carry a recognizable `km_id`).
2. **Reuse `chooseClientAlias`/`chooseServerAlias`'s own, already-correct
   alias-selection order** (`KeyManagerState::client_aliases_by_key_type`/
   `server_aliases_by_key_type`, built via `java_hashmap_iteration_order`)
   when picking which alias's identity to present, instead of a raw
   file-order-first scan of the keystore registry. This is what actually
   fixed the `test-alias`-vs-`spring-boot` mismatch.

Call sites updated: `net_phase_e.rs`'s and `phases_late.rs`'s
`SSLContext.init` native registrations, both changed to call
`resolved_identity_pem_for_key_manager_array` before falling through to
`t27_tls::attach_pending_identity_to_ctx`'s (still-present) thread-local
path.

## Verification

- `sslWithPemCertificates` alone: FAIL → **PASS**.
- Full `NettyReactiveWebServerFactoryTests` (36 methods): 2 failed → **1
  failed** (only the pre-existing, unrelated
  `whenHttp2IsEnabledAndSslIsDisabledThenH2cCanBeUsed` — see
  [`h2c-priorknowledge-hpack-headerblock-decode-failure.md`](../../known-issues/springboot/h2c-priorknowledge-hpack-headerblock-decode-failure.md),
  still OPEN, unrelated to this bug).
- Broader regression check across 12 other TLS-heavy classes
  (`JettyReactiveWebServerFactoryTests`, `JettyServletWebServerFactoryTests`,
  `SslServerCustomizerTests`, `SslMeterBinderTests`,
  `SslMetricsAutoConfigurationTests`, `TomcatReactiveWebServerFactoryTests`,
  `TomcatServletWebServerFactoryTests`, `SslConnectorCustomizerTests`,
  `PemSslStoreBundleTests`, `JksSslStoreBundleTests`,
  `AliasKeyManagerFactoryTests`, `FixedTrustManagerFactoryTests`), each run
  against a clean `dev` baseline binary and the fix binary side by side: the
  3 `HANG`s and the `JksSslStoreBundleTests`/`SslConnectorCustomizerTests`
  `FAIL`s are **pre-existing on clean `dev`**, unaffected by this fix (not
  regressions). `TomcatReactiveWebServerFactoryTests` additionally went FAIL
  → **PASS** with the fix — a bonus fix from the same root cause, not
  targeted deliberately.

## Diagnostic technique (reusable)

`CRATONVM_DBG_TLS_HS=1` and `CRATONVM_DBG_TLS_AUTH=1` (existing env-gated
`eprintln!` traces in `t27_tls.rs`/`keystore.rs`/`x509_manager.rs`/
`phases_late.rs`/`net_phase_e.rs`, extended this session with alias/store-id/
chain-length detail) print the full chronological sequence of
`KeyManagerFactory.init` → `SSLContext.init` → `createSSLEngine` calls,
including which keystore id and which specific alias each identity
resolution picked. This was essential to distinguish "wrong context gets
the identity" (ordering bug, fixed by item 1) from "right context, wrong
alias" (fixed by item 2) — the two bugs produced different rustls alerts
(`CertificateRequired` vs `DecryptError`/`BadSignature`) that would
otherwise have looked like unrelated failures. A standalone
`apps/spring-boot-suite-runner/run-single-method.ps1` script (added this
session) runs a single JUnit method through `SbRunnerMethod` in complete
process isolation — much faster than the 36-method full-class run for
iterating on a single-test hypothesis, and was what proved this was a real,
deterministic bug rather than "only reproduces with 36 tests in one
process."

## Prior session's findings (2026-07-20, concurrent worktree)

Preserved here since it reached the same core diagnosis independently and
found a genuinely separate, still-valid bug fix along the way (already on
`dev` via `c5f946ab0`, unaffected by anything in this doc):

- Traced the alert precisely: SERVER engine hit `process_new_packets ->
  Err(Some(InvalidCertificate(BadSignature)))` while unwrapping the
  CLIENT's Certificate+CertificateVerify flight; CLIENT engine then
  received `AlertReceived(DecryptError)` — a genuine CertificateVerify
  signature-validation failure, not a trust-chain rejection.
- **Found and fixed a real, separate bug**: `KeyStore.getKey(alias,
  password)`-sourced `PrivateKey` objects were never registered in
  `crypto_impl`'s `RSA_KEY_STORE`, so `Signature.sign()` on such a key
  signed with whatever unrelated key happened to occupy the same numeric
  id (a field-slot reused for a different, unrelated purpose by the native
  TLS layer). Reproduced in complete isolation
  (`Pkcs12AliasConsistencyProbe.java`, preserved under
  `../repros/pemcertificates-clientauth/`). Confirmed this fix
  alone does **not** resolve `sslWithPemCertificates` — the actual TLS
  handshake signing goes through rustls's own native `ring`-backed signer,
  never through `java.security.Signature`. Still a live, worthwhile fix for
  any code path that does `KeyStore.getKey()` then `Signature.sign()`
  directly (JWT signing, `SSLSocket`/`HttpsURLConnection` mTLS, etc).
- **Attempted and reverted**: wiring `engine_begin`'s client branch to
  prefer `JavaKeyManagerResolver` (a live `KeyManager.chooseClientAlias`
  call, mirroring the already-correct `SSLSocketFactory.createSocket` client
  path) over the single fixed `identity_override`, via a new `EngineState`
  field. This introduced a regression in
  `sslNeedsClientAuthenticationSucceedsWithClientCertificate` (a
  single-unambiguous-alias keystore): `JavaKeyManagerResolver::resolve()`
  returned `None` for that case where the `Fixed` identity path had worked,
  so the client ended up presenting no certificate at all. Reverted rather
  than trade one broken test for another.
- Correctly concluded the process-wide `RUNTIME_TLS_IDENTITY` global was
  **not** the actual mechanism — both engines already resolved a real
  per-`SSLContext` identity via `ctx_identity`, confirmed via
  `CRATONVM_DBG_TLS_AUTH` tracing — and that the alias-ambiguity gap in the
  `SSLEngine` client path (picking `spring-boot` instead of `test-alias`)
  was the much more likely mechanism, though not proven at the time.

This session's fix (above) resolves the alias-ambiguity gap without going
through `JavaKeyManagerResolver` at all — `resolved_identity_pem_for_key_manager_array`
reuses `KeyManagerState`'s already-correct, statically-precomputed alias
order instead of a live per-handshake resolver call, which is what avoids
the regression the prior attempt hit.
`sslNeedsClientAuthenticationSucceedsWithClientCertificate` was confirmed
passing both before and after this fix (it's part of the same 36-method
class's full run, see Verification above).

## Affected classes

| Module | Class | Test method |
|---|---|---|
| `module/spring-boot-reactor-netty` | `org.springframework.boot.reactor.netty.NettyReactiveWebServerFactoryTests` | `sslWithPemCertificates` |
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.reactive.TomcatReactiveWebServerFactoryTests` | (bonus fix, same root cause) |
