# `connectWithSslBundleAndOptionsMismatch` — deliberately mismatched TLS cipher suites did not fail the handshake

Status: FIXED 2026-07-21 on `fix/tls-optionsmismatch-cipherenforce-20260721`.

## Symptom

`org.springframework.boot.http.client.reactive.HttpComponentsClientHttpConnectorBuilderTests#connectWithSslBundleAndOptionsMismatch`
(both `GET` and `POST` parameterizations) failed almost every isolated run
(`-ClassList <class> -Parallel 1`, back-to-back) with:

```
java.lang.AssertionError:
Expecting code to raise a throwable.
	at org.springframework.boot.http.client.reactive.AbstractClientHttpConnectorBuilderTests.connectWithSslBundleAndOptionsMismatch(AbstractClientHttpConnectorBuilderTests.java:147)
```

The test configures the embedded Tomcat server to accept only
`TLS_AES_128_GCM_SHA256` (`webServerFactory.setSsl(ssl("TLS_AES_128_GCM_SHA256"))`)
and the client to offer only `TLS_AES_256_GCM_SHA384`
(`sslBundle(SslOptions.of(Set.of("TLS_AES_256_GCM_SHA384"), null))`) — zero
cipher overlap, so the handshake must fail with `SSLHandshakeException`. On
CratonVM the handshake succeeded instead, so the `assertThatExceptionOfType`
assertion (expecting a throwable that never came) failed.

Found as background noise (~25/30, then confirmed ~10% baseline across
combined runs) while fixing a different, unrelated bug in the same class
(`docs/internal/fixed-suite-bugs/tomcat-embedded-server-keystore-empty-cert-chain-intermittent-FIXED.md`).

## Root Cause

Two separate, independently-necessary gaps in the `SSLEngine` implementation
(`native-builtins/src/t27_tls.rs`), both on the **client** side (Apache
HttpComponents 5's `DefaultClientTlsStrategy`):

1. **`SSLEngineImpl.setSSLParameters(SSLParameters)` dropped cipher-suite
   restrictions on the floor.** Confirmed via bytecode decompilation of
   `httpclient5-5.6.1.jar`:
   `AbstractClientTlsStrategy.upgrade()` calls `engine.getSSLParameters()`,
   sets protocols/ciphers on the returned object via
   `SSLParameters.setCipherSuites([...])`, then
   `DefaultClientTlsStrategy.applyParameters()` calls
   `engine.setSSLParameters(params)` — never the legacy
   `SSLEngine.setEnabledCipherSuites(String[])` setter, which was already
   correctly wired. `register_apply_parameters`'s `setSSLParameters` handler
   only extracted ALPN protocols and the `needClientAuth`/`wantClientAuth`
   booleans from the passed `SSLParameters`; it never read
   `getCipherSuites()`. A sibling gap for the analogous `SSLSocket` path was
   already fixed on 2026-07-20 (`dfdb8bf2b`,
   "defer ClientConfig build for cipher-suite narrowing") — but that fix
   only touched `phases_late.rs`'s `SSLSocketFactory.createSocket(...)`
   plumbing, not the `SSLEngine` path used by the **reactive/async**
   connector under test here.

2. **Even with the restriction captured, `engine_begin`'s client branch
   never consumed it.** `EngineState.enabled_ciphers` (the field
   `setEnabledCipherSuites`/`setSSLParameters` populate) *is* threaded into
   the real rustls config on the **server** side
   (`build_server_config_single_cert_ex_ciphers(..., &state.enabled_ciphers)`),
   but the **client** branch called `build_client_config_ex(roots,
   &alpn_strs, ClientAuthMode::Fixed(client_auth), revocation,
   use_java_trust_manager)` — a function with no cipher-list parameter at
   all, always building the client's `rustls::ClientConfig` from the plain
   default cipher provider regardless of any restriction. Confirmed by
   inspecting Tomcat's `AbstractEndpoint.createSSLEngine` bytecode too: the
   **server** side sets ciphers via the direct
   `SSLEngine.setEnabledCipherSuites()` setter (already correctly handled)
   and only uses `SSLParameters`/`setSSLParameters` for ALPN and
   client-cert-auth flags — so gap #1 alone did not explain the failure;
   gap #2, on the client's rustls config construction, is what actually let
   the handshake succeed with a mismatched restriction.

With both gaps, the client engine's cipher-suite restriction from
`connectWithSslBundleAndOptionsMismatch`'s `SslOptions.of(Set.of(...), null)`
was silently discarded twice over: even when a caller *did* manage to record
a restriction, nothing downstream ever narrowed the actual TLS negotiation.
The client always offered its full default cipher list, which naturally
overlaps with whatever single cipher the server is restricted to, so the
handshake found a common cipher and succeeded.

## Fix

Both gaps closed in `native-builtins/src/t27_tls.rs`:

1. `register_apply_parameters`'s `SSLEngineImpl.setSSLParameters` handler
   now also calls `SSLParameters.getCipherSuites()` (via
   `ctx.invoke_virtual`, same pattern the ALPN extraction above it already
   used) and, when non-empty, stores the result into
   `EngineState::enabled_ciphers` — mirroring what
   `SSLEngine.setEnabledCipherSuites` already did directly.
2. `engine_begin`'s client (`state.is_client`) branch now builds a
   cipher-restricted `rustls::crypto::CryptoProvider` via
   `cipher_provider_for(&state.enabled_ciphers)` (the same helper the
   server branch, and the `phases_late.rs` `SSLSocket` path, already use)
   and passes it to `build_client_config_ex_with_provider(...)` instead of
   calling the unrestricted `build_client_config_ex(...)`.

## Verification

Built `cratonvm-tls-optionsmismatch-cipherenforce.exe` in a dedicated
worktree (`fix/tls-optionsmismatch-cipherenforce-20260721`, branched from
`dev`).

Isolated repro (`reactive.HttpComponentsClientHttpConnectorBuilderTests`,
`-Parallel 1`, same class, back-to-back), before vs. after the fix (only
step 2 of the fix applied for "before" — step 1 alone, verified separately,
did not resolve the failure, confirming both gaps were independently
necessary):

| Binary | Runs | `connectWithSslBundleAndOptionsMismatch` "Expecting code to raise a throwable" |
|---|---|---|
| Unfixed | 1 (both GET+POST failed) | 2/2 |
| Fixed (both gaps closed) | 45 (combined across two batches) | **0/45** |

The full 28-test class passed cleanly on every non-timed-out run after the
fix. The only residual failures across those 45 runs were two recurrences
of the *unrelated*, already-tracked keystore identity-hash residual (see
"Related, unrelated residuals" below) and a separate pre-existing hang/crash
at a fixed point unrelated to either bug — zero occurrences of this doc's
signature.

## Related, unrelated residuals found during this investigation

- **Keystore identity-hash residual** (`Private key must be accompanied by
  certificate chain`, `docs/internal/fixed-suite-bugs/tomcat-embedded-server-keystore-empty-cert-chain-intermittent-FIXED.md`):
  recurred 2/45 (~4%) even after that bug's own fix — down from the
  original ~7-8%, not fully eliminated. Flagged separately for a follow-up
  root-cause pass with live tracing (a debug-traced 20-run batch did not
  reproduce it, so no fresh trace was captured this time).
- **Silent hang/crash at a fixed point in the test class.** Roughly 20-40%
  of isolated runs of this same test class either hang (300s timeout) or
  the process crashes outright (exit code -1, no panic/stack trace in
  stderr), deterministically right after the log line `Stopping
  ProtocolHandler ["https-jsse-nio-auto-20-<port>"]` (always exactly 153
  captured stdout lines). Confirmed unrelated to this fix (reproduces
  identically before and after) and not obviously host-load-induced (still
  occurred at low concurrent process counts). Not investigated further in
  this pass — flagged separately.

## Affected classes

| Module | Class | Note |
|---|---|---|
| `module/spring-boot-http-client` | `org.springframework.boot.http.client.reactive.HttpComponentsClientHttpConnectorBuilderTests` | `connectWithSslBundleAndOptionsMismatch` (GET+POST) was failing almost every isolated run; 0/45 after fix |
