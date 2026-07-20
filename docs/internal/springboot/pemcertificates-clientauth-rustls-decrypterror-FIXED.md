# `NettyReactiveWebServerFactoryTests.sslWithPemCertificates` — rustls fatal alerts during mutual-TLS handshake

**Status: FIXED 2026-07-20.**

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

## Affected classes

| Module | Class | Test method |
|---|---|---|
| `module/spring-boot-reactor-netty` | `org.springframework.boot.reactor.netty.NettyReactiveWebServerFactoryTests` | `sslWithPemCertificates` |
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.reactive.TomcatReactiveWebServerFactoryTests` | (bonus fix, same root cause) |
