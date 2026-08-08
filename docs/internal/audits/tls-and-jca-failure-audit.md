# TLS and JCA failure audit

**Scope:** `native-builtins/` — the JSSE surface (`tls.rs`, `tls_impl.rs`,
`t27_tls.rs`, and the plaintext socket-factory bases in `phases_early.rs`) and
the JCA engine surface (`jca/signature.rs`, `jca/cipher.rs`, `crypto_impl.rs`,
`keystore.rs`, `securerandom.rs`).

**Companion:** `docs/security/crypto-failure-contract.md` states the rule and
the mechanism, for `native-builtins-crypto/` and `native-builtins-security/`.
This document applies the same rule to the crate that contract listed under
*Residual gaps* items 1 and 2. Nothing here supersedes that document; the rule
is unchanged:

> **A security API never encodes failure as ordinary output.**

For TLS the rule has a sharper form, which is the P1 item this pass closes:

> **A TLS API never returns a plaintext or no-op object.** If the requested
> protection cannot be provided, the call raises. There is no arm that returns
> a socket, an engine result, or a context.

---

## 1. TLS: what was wrong

`javax.net.ssl.SSLSocketFactory` extends `javax.net.SocketFactory`;
`javax.net.ssl.SSLServerSocketFactory` extends
`javax.net.ServerSocketFactory`. In this VM the two **base** classes carry
native registrations that build a cleartext `java.net.Socket` /
`java.net.ServerSocket`
(`phases_early::register_phase52_server_socket_factory`). In the real JDK
those base methods are either abstract or throw
`SocketException("Unsupported operation")`; here they are a working plaintext
implementation attached to the abstract class.

Native dispatch resolves up the hierarchy. **Any TLS factory overload without
its own bridge therefore inherits the plaintext body and hands the caller a
cleartext object for a call that asked for TLS.** The caller cannot tell: it
asked an `SSLServerSocketFactory` for a server socket and received a
`ServerSocket`, exactly as the API's return type promises.

This is recorded in the source itself, on both sides:

- `t27_tls.rs:4348` — *"Without explicit bridges here, dispatch reaches
  `ServerSocketFactory`'s plaintext implementation and an LDAPS client gets
  'wrong version number'."*
- `t27_tls.rs:4167` — *"Keep every `SSLServerSocketFactory.createServerSocket`
  overload on this one path so callers cannot accidentally fall through to
  `ServerSocketFactory`'s plaintext implementation."*
- `lib.rs:17503` — *"without this complete phase, a configured SSL server
  factory falls through to the plaintext `ServerSocketFactory` overloads
  (notably UnboundID LDAPS)."*

Overloads were bridged **individually, as each was found in the field**. The
failure mode of a missing bridge is silence, so that strategy can only ever
run one incident behind. The verified live gap at the start of this pass:
`SSLServerSocketFactory.createServerSocket()` (the no-arg, bind-later
overload) had **no** TLS bridge and returned a plaintext `java.net.ServerSocket`.

Separately, in synthetic-JDK mode `tls.rs`'s own `javax/net/ssl/SSLEngine`
performs no cryptography at all — its own doc comment says so — yet
`wrap`/`unwrap` returned `Status.OK`, reached `HandshakeStatus.FINISHED`, and
the session reported `TLSv1.3` / `TLS_AES_256_GCM_SHA384`. An application that
wrapped plaintext and wrote the (untouched) destination buffer to a socket
sent cleartext while every JSSE status it could observe said the handshake had
completed.

## 2. TLS entry-point table

| Entry point | Unsupported path | Old behaviour | New behaviour |
|---|---|---|---|
| `javax.net.SocketFactory.createSocket()` (base) | receiver is an `SSLSocketFactory` (or subclass) whose `()` overload has no bridge | plaintext `java.net.Socket` | **`SSLException`** — `tls_deny::deny_plaintext_fallback` |
| `javax.net.SocketFactory.createSocket(String,int)` | as above | plaintext connected `Socket` | **`SSLException`** |
| `javax.net.SocketFactory.createSocket(InetAddress,int)` | as above | plaintext connected `Socket` | **`SSLException`** |
| `javax.net.SocketFactory.createSocket(String,int,InetAddress,int)` | as above | plaintext connected `Socket` | **`SSLException`** |
| `javax.net.SocketFactory.createSocket(InetAddress,int,InetAddress,int)` | as above | plaintext connected `Socket` | **`SSLException`** |
| `javax.net.ServerSocketFactory.createServerSocket()` | **live gap** — no TLS bridge exists for the no-arg (bind-later) overload | plaintext `java.net.ServerSocket` | **`SSLException`** |
| `javax.net.ServerSocketFactory.createServerSocket(int)` | receiver is an `SSLServerSocketFactory` and the bridge is missing/overwritten | plaintext `ServerSocket` | **`SSLException`** |
| `javax.net.ServerSocketFactory.createServerSocket(int,int)` | as above | plaintext `ServerSocket` | **`SSLException`** |
| `javax.net.ServerSocketFactory.createServerSocket(int,int,InetAddress)` | as above | plaintext `ServerSocket` | **`SSLException`** |
| `SSLContext.getInstance(String)` (`tls.rs`, synthetic-JDK) | any unrecognised protocol name | `_ => 0` ⇒ an ordinary **"TLS" context**, and `getProtocol()` reported "TLS" | **`NoSuchAlgorithmException`**; recognised names now each carry their own index so `getProtocol()` echoes the request |
| `SSLContext.getInstance(null)` (`tls.rs`) | null protocol | silently defaulted to "TLS" | **`NullPointerException`** |
| `SSLContext.getSocketFactory()` (`tls.rs`) | context never `init()`ed | a factory with **no key/trust managers**, indistinguishable from a configured one | **`IllegalStateException`** (real JSSE's wording and type) |
| `SSLContext.getServerSocketFactory()` (`tls.rs`) | as above | as above | **`IllegalStateException`** |
| `SSLEngine.wrap(ByteBuffer[],ByteBuffer)` (`tls.rs`) | always — the engine has no cryptography | `Status.OK`, 0 bytes produced, handshake advanced | **`SSLException`**, unless `CRATONVM_ALLOW_NONCRYPTO_SSLENGINE=1` |
| `SSLEngine.unwrap(ByteBuffer,ByteBuffer[])` (`tls.rs`) | always | `Status.OK` and eventually `HandshakeStatus.FINISHED` — a completed TLS 1.3 handshake that never happened | **`SSLException`**, same opt-in |

### Entry points inspected and deliberately left unchanged

| Entry point | Why it stays |
|---|---|
| `SSLEngine.wrap`/`unwrap` **after `closeOutbound()`/`closeInbound()`** | **PRESERVED NEGATIVE.** `Status.CLOSED` is a real, correct answer — the engine was closed and there genuinely is nothing more to send. It claims no protection, so it stays a value. Both early-return arms are before the deny check and are covered by a test. |
| `SSLServerSocketFactory.createServerSocket(int)` / `(int,int)` / `(int,int,InetAddress)` in `t27_tls.rs` | Real TLS listeners. They already refuse when no identity is configured (`require_runtime_tls_identity()?` ⇒ `IllegalStateException("No TLS key/cert configured…")`) and when the rustls **and** platform/OpenSSL configs both fail (`IOException`). No plaintext arm. |
| `SSLSocketFactory.createSocket(..)` × 6 in `phases_late/ssl_security.rs` | Real TLS sockets on every arm; the layering overload refuses a factory with no owning `SSLContext` (`IllegalStateException`). |
| `SSLContext.getInstance` in `net_phase_e.rs` (real-JDK mode) and `phases_late/ssl_security.rs` | Both **already** validate the protocol name and refuse. `tls.rs` was the odd one out; it is now consistent with them. |
| `sun.security.ssl.SSLEngineImpl.wrap`/`unwrap` (`t27_tls.rs`) | The genuine rustls-backed engine. Registered on a **different class** from `tls.rs`'s stub, so the two never shadow each other, and it is unaffected by the `CRATONVM_ALLOW_NONCRYPTO_SSLENGINE` gate. |
| `tls_impl.rs` | A TLS 1.3 protocol kernel (record layer, key schedule, `TlsAlert`, session tickets) gated behind `legacy-synthetic-crypto`. Every fallible operation already returns `Result<_, TlsError>`; `CipherSuite::from_code`/`NamedGroup::from_code`/`SignatureAlgorithm::from_code`/`ContentType::from_code`/`AlertDescription::from_code` are closed `Option`-returning lookups with **no default variant**, so no unknown code can be coerced to a supported one. Nothing to change. |

## 3. What is now deny-by-default

`native-builtins/src/tls_deny.rs` (new) holds the guard and the **explicit
opt-in lists**:

| Item | Meaning |
|---|---|
| `BRIDGED_SSL_SOCKET_FACTORY_OVERLOADS` | the six `SSLSocketFactory.createSocket` descriptors that have a real TLS bridge |
| `BRIDGED_SSL_SERVER_SOCKET_FACTORY_OVERLOADS` | the three `SSLServerSocketFactory.createServerSocket` descriptors that bind a real TLS listener |
| `UNBRIDGED_SSL_SERVER_SOCKET_FACTORY_OVERLOADS` | the descriptors this VM knowingly does not implement — currently just `createServerSocket()` |
| `deny_plaintext_fallback` | called as the **first statement** of all nine plaintext base bodies |

A TLS receiver reaching a plaintext base body means, by construction, that no
bridge matched that `(class, method, descriptor)` triple — if one had, it would
have won dispatch and the base body would never have run. So the guard needs no
run-time table lookup to decide; the lists exist to make the same statement
checkable at **test** time.

The opt-in property is enforced by two tests:

- `base_overloads_are_bridged_or_explicitly_denied` — every descriptor in
  `BRIDGED_*` really is registered on the TLS class, and every descriptor in
  `UNBRIDGED_*` really is not. The lists cannot become a lie.
- `every_plaintext_base_overload_is_accounted_for` — every plaintext base
  overload appears in exactly one of the lists.

**Consequence:** adding a new overload to `javax/net/SocketFactory` or
`javax/net/ServerSocketFactory` without either bridging it for TLS or recording
it as knowingly unsupported fails the test suite; and if it somehow ships, the
first TLS call to it raises instead of returning cleartext.

### Which exception, and why

`javax.net.ssl.SSLException` — a subclass of `IOException`, which both
`createSocket` and `createServerSocket` already declare, so an existing
`catch (IOException)` still handles it and the refusal cannot escape past a
caller written to handle connection failure. It is also unmistakably a TLS
failure, which `SocketException("Unsupported operation")` (the real JDK's
wording for the no-arg case) is not. `UnsupportedOperationException` was
rejected for the opposite reason: unchecked, so it escapes the `catch` that
server-startup code is built around, turning a handled refusal into a crash.

The fallback arm, for a build where `javax/net/ssl/SSLException` cannot be
constructed, is a plain `IOException`. **There is deliberately no arm that
returns a value** — the same shape as `phases_early::throw_jca_exc`
(`native-builtins/src/phases_early.rs:14672`), the mechanism named in the
companion contract.

## 4. JCA audit table

### 4.1 Sites changed

| # | Site | Old behaviour | New behaviour | JDK-specified exception | Justification |
|---|---|---|---|---|---|
| 1 | `jca/signature.rs` `sig_sign` | `sign_dispatch(..).unwrap_or_default()` ⇒ an **empty `byte[]` returned as the signature** | `None` ⇒ raise | `java.security.SignatureException` | The archetype. `sign()` reported success and the caller shipped an unsigned JWS/JAR/token. `SignatureException` is what `sign()` declares, so every caller can catch it. |
| 2 | `jca/signature.rs` `sig_sign_into` | same `.unwrap_or_default()`, then `written = 0` | `None` ⇒ raise | `java.security.SignatureException` | "Signed, zero bytes" and "not signed" were the same return value. |
| 3 | `jca/signature.rs` `sig_verify` | `verify_dispatch(..).unwrap_or(false)` | `Some(b)` ⇒ `b`; `None` ⇒ raise | `java.security.SignatureException` | **The P0 the report names.** `crypto_impl::rsa_verify` is `guard.get(&id).map(..)`, so `None` is unambiguously *"no such key"* and `Some(false)` is *"checked, and it does not match"*. `.unwrap_or(false)` reported an unusable key as a **forged signature**. |
| 4 | `jca/signature.rs` `sig_verify_off_len` | same | same | `java.security.SignatureException` | As #3. |
| 5 | `jca/signature.rs` `natively_dispatched` + `refuse_unanswerable` (new) | (absent) | classifies *why* the dispatch was `None` — no backend for the algorithm vs. key handle absent — and raises with the matching wording | — | Two different defects must not produce the same diagnostic. Kept in lock-step with the dispatch tables by `natively_dispatched_matches_the_dispatch_arms`. |
| 6 | `jca/cipher.rs` `cipher_init_record` | `rsa_key_components(..).unwrap_or_default()` ⇒ an **empty modulus/exponent recorded as the cipher's key**; `init` reported success | unusable ⇒ raise, before any state is recorded | `java.security.InvalidKeyException` | `cipher_do_final_impl:1017` did catch the empty pair, but at `doFinal` and as `IllegalStateException` — neither where nor what the JDK specifies. `Cipher.init` declares `InvalidKeyException` precisely so an unusable key is rejected at init. Scoped to RSA transformations; an empty pair is legitimate and unused for every other transformation (`non_rsa_cipher_init_is_unaffected_by_the_rsa_key_guard`). |
| 7 | `jca/cipher.rs` `Cipher.doFinal([BII[B)I` | `Ok(_) => Vec::new()` ⇒ **0 bytes written, `return 0`** for an unexpected result shape | raise | `java.lang.IllegalStateException` | `cipher_do_final_impl` only ever returns `Err` or a real `byte[]`, so this arm meant "something is wrong", not "the ciphertext is empty". Reporting a successful zero-length encryption is the same bug class as #1. |
| 8 | `securerandom.rs` `native_secure_random_next_long` | low half `sha1prng_next(..).unwrap_or(0)` ⇒ a `SecureRandom` draw whose **bottom 32 bits are a constant** | fall through to the OS-entropy path, which raises `SecurityException` if entropy is unavailable | `java.lang.SecurityException` | The high half having succeeded makes a low-half failure near-impossible — but "near-impossible" is not a property an RNG may rely on, and there was no signal. Now consistent with the V2 fix already applied to the same function's tail and to `next_bytes`. |
| 9 | `tls.rs` `SSLContext.getInstance` | unknown protocol ⇒ `_ => 0` ⇒ a working "TLS" context | raise | `java.security.NoSuchAlgorithmException` | See the TLS table. Listed here too because it is a JCA `getInstance` contract. |
| 10 | `tls.rs` `SSLContext.getSocketFactory` / `getServerSocketFactory` | factory handed out for an un-`init()`ed context | raise | `java.lang.IllegalStateException` | Real JSSE (`SSLContextImpl.engineGetSocketFactory`) throws exactly this. `getDefault()` marks its context initialised, so the JDK's documented default path is unaffected (`get_default_context_can_still_hand_out_a_socket_factory`). |
| 11 | `tls.rs` `SSLEngine.wrap` / `unwrap` | fabricated a successful TLS 1.3 handshake with no cryptography | raise, unless explicitly opted in | `javax.net.ssl.SSLException` | See §2. |
| 12 | `crypto_impl.rs` `Rsa::verify_sha256` → new `Rsa::try_verify_sha256` | called `native-builtins-crypto`'s ambiguous `verify_rsa_pkcs1_v15` (`-> bool`), so a key the backend **refuses** (even exponent, `e < 2`, `e > 2³³−1`, modulus > 4096 bits, absent component) and a signature that **does not match** were the same `false` | `try_verify_sha256` returns `Result<bool, CryptoFailure>`; `verify_sha256` keeps its `bool` surface and delegates **fail-closed** (`matches!(.., Ok(true))`) | — | See §4.3. No public signature removed or renamed; the `bool` form is still what certificate-chain validation uses. |
| 13 | `crypto_impl.rs` `rsa_verify` | `.map(Rsa::verify_sha256)` ⇒ a refused key surfaced as `Some(false)` | `try_verify_sha256(..).ok()` ⇒ refusal becomes `None`, which `jca::signature` turns into a `SignatureException` | `java.security.SignatureException` (at the facade) | This is what makes #3 complete: without it, `verify_dispatch` would still have received `Some(false)` for a legitimate 8192-bit signer key. |
| 14 | `phases_early.rs` `Signature.verify([B)Z` (legacy synthetic) | three `.unwrap_or(false)` arms (Ed25519 / ECDSA / RSA) | `Some(b)` ⇒ `b`; `None` ⇒ raise, via the new `verify_never_ran` helper | `java.security.SignatureException` | The same conflation as #3, in the second `Signature` implementation. `Some(false)` is untouched. |
| 15 | `phases_early.rs` `Signature.verify([B)Z`, null-key arm | `_ => false` ⇒ a Signature with no verification key reported the signature **invalid** | raise | `java.security.InvalidKeyException` | `initVerify(null)` is an invalid key, not a failed verification. |

### 4.2 Sites inspected and deliberately left unchanged

| Site | Why it stays |
|---|---|
| `jca/signature.rs` `verify_dispatch` returning `Some(false)` | **PRESERVED NEGATIVE.** The signature really was checked against the key and really did not match. That is the security decision the caller asked for. Covered by `verify_dispatch_distinguishes_never_checked_from_did_not_match` and `native_sign_verify_round_trip_and_genuine_mismatch_is_false`. |
| `jca/signature.rs` `sig_get_algorithm` | Already annotated in source: a benign accessor, deliberately kept lenient so a caller that only reads the algorithm name after an unrelated state bug gets an answer rather than an exception cascade. It makes no cryptographic claim. |
| `jca/signature.rs` real-SPI routes (ECDSA / EdDSA / DSA / ML-DSA) | These bypass `sign_dispatch`/`verify_dispatch` entirely and drive the real JDK SPI, whose own `engineVerify` returns the genuine boolean and whose failures are already real exceptions. `drive_real_signature_spi` already raises `IllegalStateException` for a missing key and `NotImplemented` for an absent SPI class. |
| `keystore.rs` `keystore_has_alias` (`:2511`) | `unwrap_or(false)` on an **unregistered store id**. Its documented job is to verify a mutation landed, and its caller maps `false` to `KeyStoreException` — so the `false` is already routed to a loud failure, not swallowed. |
| `keystore.rs` `KeyStore.containsAlias` / `getEntry` / `getCertificate` returning absent | **PRESERVED NEGATIVE.** "This alias is not in this keystore" is the answer to the question asked. `containsAlias` returning `false` for a real absence, and the `Ok(Some(Value::Object(None)))` arms for a genuinely absent entry, are the JDK contract (`getKey`/`getCertificate` return `null` for an unknown alias). |
| `keystore.rs` chain-building `parse_certificate(..).unwrap_or(false)` (`:657`, `:665`) | A **search predicate**, not a cryptographic result: "does this candidate DER parse and does its subject match the issuer we are looking for?". An unparseable candidate is skipped and the chain simply ends there; the chain is later validated by `x509_manager`. No trust decision is made on the `false`. |
| `securerandom.rs` `set_entropy_seed` time-based fallback (`:248`–`:260`) | Reached only from `native_random_init_noseed`, i.e. **`java.util.Random`'s** no-arg constructor — not `SecureRandom`. `java.util.Random` is documented as not cryptographically strong; a time+counter seed is the JDK's own shape there. `SecureRandom`'s own paths (`next_bytes`, `next_long`, `next_int`) all raise `SecurityException` on entropy failure. |
| `securerandom.rs:1569` `os_random_u64().unwrap_or(0)` | Inside `#[cfg(test)]` (`test_secure_random_next_int_bound_is_unbiased_enough`). Not a production path. |
| `jca/cipher.rs` `is_rsa_transformation` / `pad_str` `.unwrap_or(false)` (`:334`, `:1086`) | Transformation-**string parsing**, not a crypto result. `false` means "this name is not RSA" / "this name does not say NoPadding", which is the correct reading of an absent component, and the defaults it selects (`PKCS5Padding`) are the JDK's own. |
| `jca/cipher.rs` `cipher_init_record_pbes2` fallback to `cipher_init_record` | An unrecognised PBES2 name or an un-`init`ed `AlgorithmParameters` falls back to the pre-existing raw-key path, which then runs the same guards. Documented in source as "behaves as before, not worse"; narrowing it needs the full PBES2 parameter surface and is listed as a residual. |
| `crypto_impl.rs` `Rsa::verify_sha256` (the `bool` surface) | Retained deliberately — see §4.3. Certificate-chain validation has no exception channel and treats both outcomes identically; the collapse is now explicit, documented, and covered by `the_bool_verify_surface_is_still_fail_closed`. |
| `crypto_impl.rs` remaining primitives | The fallible ones already return `Option`/`Result`; the collapses were all at the **callers** (#1–#4, #6, #13–#15). Changing the primitives' signatures would ripple through callers this pass may not edit. The `Option` contract is now load-bearing and is asserted directly by `verify_dispatch_distinguishes_never_checked_from_did_not_match` and `rsa_verify_separates_a_refused_key_from_a_failed_verification`. |

### 4.3 The ambiguous `bool` wrapper

The companion contract's residual #5 names
`classloading/src/jar_signer.rs:1516` as the un-migrated caller of
`native-builtins-crypto`'s `verify_rsa_pkcs1_v15` (`-> bool`).

**There was a second call site, and it was in this crate:**
`crypto_impl.rs:2225`, inside `Rsa::verify_sha256` — which is the RSA backend
for `Signature.verify()` via `crypto_impl::rsa_verify`. It has been migrated
(table items 12–13):

- `Rsa::try_verify_sha256` (new) calls `verify_rsa_pkcs1_v15_checked` and
  returns `Result<bool, CryptoFailure>`.
- `Rsa::verify_sha256` keeps its `bool` signature — nothing public was removed
  or renamed — and delegates fail-closed with `matches!(.., Ok(true))`. It is
  the right surface for its remaining callers: certificate-chain validation
  (`crypto_impl.rs:3914`, `x509_manager`, `checkServerTrusted`), where a
  refusal and a mismatch both mean "do not trust this chain" and there is no
  exception channel to report the difference through.
- `crypto_impl::rsa_verify` uses the checked form, so a refused key now
  produces `None` rather than `Some(false)`, and `jca::signature` raises a
  `SignatureException` instead of reporting a forgery.

**`classloading/src/jar_signer.rs:1516` remains un-migrated and is not this
crate's to change.** The wrapper is fail-closed, so the behaviour is safe and
only the *reporting* is imprecise: an unusable signer key is reported as
`SigVerify::Bad` rather than the `SigVerify::Unsupported` variant that already
exists.

## 5. Test coverage

All tests are `#[cfg(test)]` inside `native-builtins/`. Every "must raise"
property has a "must still work" twin, so the suite cannot be satisfied by
rejecting everything.

| Property required | Test |
|---|---|
| A plaintext factory still works | `tls_deny::tests::plain_socket_factory_receiver_is_allowed_through`, `plain_server_socket_factory_receiver_is_allowed_through` |
| A TLS factory can never reach a plaintext body | `tls_deny::tests::ssl_socket_factory_receiver_is_denied`, `ssl_server_socket_factory_receiver_is_denied` |
| Application wrappers are covered too | `tls_deny::tests::an_application_subclass_of_an_ssl_factory_is_denied` |
| The guard does not false-positive | `tls_deny::tests::an_unrelated_class_is_not_mistaken_for_a_tls_factory`, `a_static_call_with_no_receiver_is_left_alone` |
| The refusal is a TLS exception, not a socket | `tls_deny::tests::the_refusal_names_tls_and_is_not_a_socket` |
| **Adding an overload requires an explicit opt-in** | `tls_deny::tests::base_overloads_are_bridged_or_explicitly_denied`, `every_plaintext_base_overload_is_accounted_for` |
| Supported TLS protocols still work, and are echoed back | `tls::tls_tests::ssl_context_get_instance_accepts_every_supported_protocol_and_echoes_it_back`, `ssl_context_get_instance_is_case_insensitive` |
| Unsupported protocol raises | `tls::tls_tests::ssl_context_get_instance_refuses_an_unsupported_protocol`, `the_unsupported_protocol_refusal_is_a_no_such_algorithm_exception`, `ssl_context_get_instance_of_null_is_a_null_pointer_exception` |
| Uninitialised context raises; initialised one still works | `tls::tls_tests::get_socket_factory_on_an_uninitialized_context_raises`, `get_socket_factory_after_init_succeeds`, `get_default_context_can_still_hand_out_a_socket_factory` |
| A no-op engine cannot report a handshake; the opt-in still works; `CLOSED` stays a value | `tls::tls_tests::noncrypto_engine_denies_by_default_works_when_opted_in_and_still_reports_closed` |
| Sign/verify round-trip still works | `jca::signature::tests::native_sign_verify_round_trip_and_genuine_mismatch_is_false` |
| **Genuine mismatch still returns `false`, no exception** | same test (second half), and `jca::signature::tests::verify_dispatch_distinguishes_never_checked_from_did_not_match` |
| Unusable key raises instead of `false` / empty signature | `jca::signature::tests::native_verify_with_an_unusable_key_raises_instead_of_returning_false`, `native_sign_with_an_unusable_key_raises_instead_of_returning_an_empty_signature` |
| Unsupported algorithm is never reported as a mismatch | `jca::signature::tests::an_algorithm_with_no_backend_is_never_reported_as_a_mismatch` |
| The refusal classifier tracks the dispatch tables | `jca::signature::tests::natively_dispatched_matches_the_dispatch_arms` |
| A backend-refused key is `None`, a mismatch is `Some(false)`, a real key still verifies | `crypto_impl::tests::rsa_verify_separates_a_refused_key_from_a_failed_verification` |
| The retained `bool` verify surface stays fail-closed | `crypto_impl::tests::the_bool_verify_surface_is_still_fail_closed` |
| `Cipher.init` rejects an unusable RSA key; a real one still works; other transformations unaffected | `jca::cipher::tests::rsa_cipher_init_with_an_unusable_key_raises_invalid_key`, `rsa_cipher_init_with_a_real_key_succeeds`, `non_rsa_cipher_init_is_unaffected_by_the_rsa_key_guard` |

## 6. Residual gaps

Ordered by exposure.

1. **`phases_early::cipher_do_final` still returns `null` from
   `Cipher.doFinal` on an uninitialised Cipher** (`phases_early.rs:14701`),
   explicitly *instead of* `IllegalStateException`, and reads the algorithm
   name and key bytes with `unwrap_or_default()` / `Vec::new()` at `:14706`
   and `:14714`, so a missing key becomes an **empty key** rather than an
   `InvalidKeyException`. This is the degradation the companion contract named
   as residual #2, and it is real. It was **not** changed here because its
   in-source comment states that the integration test
   `cipher_do_final_passthrough` asserts the `null`, and that test could not be
   run in this pass. Fixing it means changing the native and that test in the
   same commit. `jca/cipher.rs`'s own `cipher_do_final_impl` — the other
   registration for the same method — already raises correctly; determining
   which of the two wins dispatch (registry semantics are last-writer-wins)
   should be the first step.

2. **`tls.rs`'s synthetic `SSLSession` still reports an optimistic session.**
   `getCipherSuite()` returns `TLS_AES_256_GCM_SHA384`, `getProtocol()`
   returns `TLSv1.3` and `isValid()` returns `true` for a session no handshake
   ever produced. With `wrap`/`unwrap` now refusing, no data can flow through
   this engine, so the session cannot be used to *claim* protection over real
   bytes — but a caller that inspects `getSession()` without moving data still
   reads a fabricated TLS 1.3 session. Real JSSE reports the null session
   (`SSL_NULL_WITH_NULL_NULL` / `NONE` / `isValid() == false`) until a
   handshake completes. Fixing it is a small change to
   `init_ssl_session_fields` plus the `getCipherSuite`/`getProtocol` arms, and
   was left out of this pass to keep the change set to failure semantics.

3. **The guard cannot see a factory whose `ClassId` was lost.**
   `receiver_is_tls_factory` matches by `ClassId` (with an exact-name
   backstop), because `alloc_concurrent_synthetic` documents that
   `class_name_of_id` can misreport an interface-like synthetic class as
   `java/lang/Object` — and both JSSE factories are abstract. If
   `javax/net/ssl/SSLSocketFactory` itself fails to load, that function's
   `Err` arm allocates against `ClassId::new(0)` and the guard sees
   `java/lang/Object`. In real-JDK mode both classes always load; in
   synthetic-JDK mode they are `ensure_synthetic_class`-created. No live path
   is known to hit this, but it is the one way a TLS receiver can still slip
   past.

4. **`SSLServerSocketFactory.createServerSocket()` is refused, not
   implemented.** The bind-later shape has no TLS listener object to hand
   back, because `t27_tls::create_ssl_server_socket` binds the `TcpListener`
   and builds the rustls config in one step. Implementing it means splitting
   that function into "prepare config" and "bind", and giving
   `SSLServerSocket.bind(SocketAddress)` a real body.

5. **`SSLEngine` opt-in latches.** `CRATONVM_ALLOW_NONCRYPTO_SSLENGINE` is
   read once into a process-wide tri-state, so a later `set_var` is invisible.
   That is deliberate (it matches the crate's other kill-switches) and the
   tests use a `#[cfg(test)]` setter rather than the environment, but it does
   mean the flag must be set before the VM starts.

6. **`cipher_init_record_pbes2`'s fallback is still lenient.** An
   unrecognised `PBEWithHmacSHA*AndAES_*` variant or an un-`init`ed
   `AlgorithmParameters` silently falls back to using the raw password bytes
   as the key rather than raising. It is not *worse* than before and the
   downstream cipher still runs its own guards, but "we could not derive the
   key, so we used the password" is a degradation the rule would refuse.
   Narrowing it needs the SHA-384/512 PRF variants implemented in
   `pbkdf2_derive_for` first, so that a refusal cannot break a case that
   currently works.

7. **Certification-path validation for signed JARs** — unchanged from the
   companion contract §4.2. Until it exists, signed JARs are not a trust
   boundary, regardless of how precisely `Signature.verify()` now reports.

8. **`classloading/src/jar_signer.rs:1516`** — see §4.3. Not this crate's to
   change.
