# Cryptographic failure contract

**Scope:** `native-builtins-crypto/` and `native-builtins-security/`. The same
rule governs the JSSE and JCA surface in `native-builtins/` (`tls.rs`,
`tls_impl.rs`, `t27_tls.rs`, `jca/signature.rs`, `jca/cipher.rs`,
`crypto_impl.rs`, `keystore.rs`, `securerandom.rs`), where it takes a sharper
form: **a TLS API never returns a plaintext or no-op object.** A socket factory
that cannot negotiate TLS must fail, not hand back a cleartext socket.
**Status:** audit complete for those two crates; the JCA/TLS engine surface in
`native-builtins/` is **out of scope for this pass** and is listed under
[Residual gaps](#residual-gaps).

---

## 1. The rule

> **A security API never encodes failure as ordinary output.**

An empty `byte[]` from `Signature.sign()`, a `false` from `Signature.verify()`
because the *key* was rejected, a zero block from a cipher whose key schedule
was malformed, a silently truncated private-key scalar — each of these is
indistinguishable, at the call site, from a legitimate negative security
decision. The caller records "not signed", "did not verify", "no match" when
the truth is *"we never checked"*. That is the bug class this document closes.

Three consequences, applied throughout both crates:

1. **Unsupported / uninitialised / malformed ⇒ raise.** The kernel returns an
   error naming the JDK-specified exception; the facade throws it. Never a
   default, never a zero, never `Ok(())`.
2. **Genuine negatives are preserved.** `verify()` returning `false` for a real
   digest mismatch is *correct* and must not become an exception. Corrupt
   signature *bits* are what a forgery looks like — that is a `false`. A wrong
   signature *length* is a malformed encoding — that is a `SignatureException`.
   The two are distinguished explicitly, and each decision is commented at its
   site.
3. **Failure is catchable.** Errors carry a `java.*` class name and are thrown
   as real `Throwable`s. `Error`/panic is not an acceptable failure mode for a
   condition Java code is expected to handle.

### The mechanism

`native-builtins-crypto` is a **pure kernel island** with no `NativeContext`
(see its `lib.rs`): it cannot construct a Java `Throwable`, so it returns
`Err(CryptoFailure)` carrying the internal-form exception class name, and the
facade throws it. `native-builtins-security` *does* have a `NativeContext` and
throws directly.

Both mirror the facade's existing helper — **copied from
`native-builtins/src/phases_early.rs:14672` (`throw_jca_exc`)**, cross-checked
against `native-builtins/src/phases_late/bouncycastle.rs:6040`
(`bc_gost_throw_crypto_exception`):

```rust
let detail = ctx.create_string(msg);
match ctx.new_object_initialized(class_name, "(Ljava/lang/String;)V",
                                 &[Value::Object(Some(detail))]) {
    Ok(Some(Value::Object(Some(exc)))) => MethodCallFailed::ExceptionThrown(exc),
    _ => RuntimeError::IllegalStateException { .. }.into(),   // still loud
}
```

The fallback arm is load-bearing: if the exception class itself cannot be
constructed (synthetic-JDK mode, class absent from the boot path) an unchecked
exception is still raised. **There is deliberately no arm that returns a
value.**

| Where | Item |
|---|---|
| `native-builtins-crypto/src/failure.rs` | `CryptoFailure` + the JDK class-name constants (new file) |
| `native-builtins-security/src/sunec_point.rs:97` | `throw_jca` — the direct-throw helper |
| `native-builtins-security/src/sunec_point.rs:82` | `PROVIDER_EXCEPTION` constant |

---

## 2. Audit table

### 2.1 Sites changed

| # | Site | Old behaviour | New behaviour | JDK-specified exception | Justification |
|---|---|---|---|---|---|
| 1 | `native-builtins-crypto/src/signature.rs:32` (was) → `:91` | `RsaPublicKey::new` fails ⇒ `return false` | `Err` naming the rejection | `java.security.InvalidKeyException` | **P0.** The backend rejects an even exponent, `e < 2`, `e > 2³³−1`, and any modulus over `RsaPublicKey::MAX_SIZE` (**4096 bits**). A *legitimate* 8192-bit signer key therefore verified as `false` — reported as "signature did not verify" when nothing was checked. On a signed-JAR path that is a trust decision made on no evidence. |
| 2 | `native-builtins-crypto/src/signature.rs:75` | (absent — folded into #1) | Empty/zero modulus ⇒ `Err` | `java.security.InvalidKeyException` | An absent key component is not a key. Rejected before the backend so the message names the defect. |
| 3 | `native-builtins-crypto/src/signature.rs:80` | (absent) | Empty/zero exponent ⇒ `Err` | `java.security.InvalidKeyException` | As #2. |
| 4 | `native-builtins-crypto/src/signature.rs:99` | Wrong-length signature ⇒ backend `Err` ⇒ `false` | `Err` with the SunRsaSign wording | `java.security.SignatureException` | SunRsaSign's `engineVerify` throws `SignatureException("Signature length not correct: got N but was expecting K")` *before* any RSA operation. A genuine mismatch always has the correct length, so this can never swallow a real negative. |
| 5 | `native-builtins-crypto/src/signature.rs:125` | `false` on padding/digest mismatch | **unchanged** — `Ok(false)` | *none* | **PRESERVED NEGATIVE.** This is the real security decision. Verified by `genuine_mismatch_is_ok_false_not_an_error` and `corrupt_signature_bits_are_ok_false`. |
| 6 | `native-builtins-crypto/src/signature.rs:145` | `verify_rsa_pkcs1_v15-> bool` | kept, now delegates and fails **closed** (`matches!(.., Ok(true))`) | — | Signature preserved for the un-migrated caller at `classloading/src/jar_signer.rs:1516`. `Err` can never surface as `true`; verified by `bool_wrapper_is_fail_closed_on_every_error_path`. |
| 7 | `native-builtins-crypto/src/bc_aes.rs:187,213,220` | Malformed key schedule ⇒ `kw.len() - 1` underflow, or an out-of-bounds `kw[rounds]` on the final round ⇒ panic | `try_encrypt_block`/`try_decrypt_block` raise; output left untouched | `java.lang.IllegalStateException` | A block cipher must never write a zero block for an unkeyed engine. **New finding:** the guard is length-**exact** (11/13/15) rather than the `kw.len() >= 2` lower bound the existing callers use — that bound still admits `kw.len() == 2` or `4`, which the round structure indexes past. See gap #4. |
| 8 | `native-builtins-crypto/src/bc_aes.rs:413` | `generate_working_key -> None` (already loud) | `try_generate_working_key` attaches the exception name | `java.lang.IllegalArgumentException` (BC's own: *"Key length not 128/192/256 bits."*) | Only 16/24/32-byte keys are accepted; no zero-pad or truncate fallback. |
| 9 | `native-builtins-crypto/src/bc_chacha.rs:57,72,80,88` | Odd `rounds` ⇒ loop runs `rounds−1` ⇒ **plausible but wrong keystream**, silently | `try_chacha_core`/`try_salsa_core`/`try_permute` raise | `java.lang.IllegalArgumentException` (BC's own: *"Number of rounds must be even"*) | The archetype of failure-as-output: a keystream that is not the keystream, with no error anywhere. |
| 10 | `native-builtins-security/src/sunec_point.rs:237` | `if let Value::Int(b)` — a non-`Int` element left the scalar byte at its initialised **zero** | `match` with an error arm | `java.lang.IllegalArgumentException` | **Silent private-key corruption.** A partially-zeroed scalar yields a well-formed *wrong* point that is then signed/key-agreed with. A byte we could not read is not a byte worth guessing. |
| 11 | `native-builtins-security/src/sunec_point.rs:274` | `class_name_of_id(..).unwrap_or_default()` ⇒ `""` ⇒ fell into the unsupported-curve arm with a blank name | explicit `None` arm with its own message | `java.security.ProviderException` | Two different refusals must produce two different diagnostics. |
| 12 | `native-builtins-security/src/sunec_point.rs:291` | unsupported curve ⇒ `IllegalArgumentException` (loud, but not the provider contract) | `ProviderException` via `throw_jca` | `java.security.ProviderException` | The JDK's own unchecked type for "a provider engine accepted the request and then could not complete it"; correct for an invariant violation inside `sun.security.ec`, and needs no `throws` clause on `ECOperations.multiply`. |
| 13 | `native-builtins-security/src/sunec_point.rs:437` (`impl_curve_scalar_mul`) | `s_le.iter().take($nbytes)` silently discarded high-order scalar bytes | significant excess bytes ⇒ `None` ⇒ caller raises | `java.lang.IllegalArgumentException` | `s` and `s mod 2^(8·n)` were indistinguishable: the multiply answered a question the caller never asked, with a well-formed point. Trailing **zero** padding (BigInteger sign byte) is still accepted — verified by `zero_padded_oversized_scalar_is_still_accepted`, so the guard cannot false-positive. |

### 2.2 Sites inspected and deliberately left unchanged

| Site | Why it stays |
|---|---|
| `native-builtins-crypto/src/signature.rs:125` | See #5 — the preserved negative. |
| `native-builtins-crypto/src/bc_aes.rs:238,309` (`encrypt_block`/`decrypt_block`) | Signature is `-> ()`; changing it would break the four `native-builtins` registrations this pass may not edit. Behaviour is unchanged (still panics on a malformed schedule — *loud*, never ciphertext-shaped), and all four callers pre-validate, if only with a `>= 2` lower bound: `phases_late/bouncycastle.rs:6081`, `:6199`, `:6565`, plus `:5837` for the schedule itself. `# Panics` now documented; migration to `try_*` is gap #4. |
| `native-builtins-crypto/src/bc_chacha.rs` infallible kernels | Same reasoning; all three callers pre-validate parity (`phases_late/bouncycastle.rs:7233`, `:7260`, `:7284`). |
| `native-builtins-crypto/src/bc_newhope.rs` | Pure lattice arithmetic over fixed-size `[i16; 1024]`. No algorithm selection, no key parsing, no unsupported-input path. `uniform`'s rejection sampler always fills exactly `N` coefficients. Nothing to degrade. |
| `native-builtins-security/src/sunec_intpoly.rs:91` | `iter_u64_digits().next().unwrap_or(0)` — **not** a swallowed error. `BigUint` stores zero as an empty digit vector, so `None` occurs for exactly one value (zero) and `0` *is* that value. Annotated in-line; covered by the `0 · x` vector in `mult_matches_jdk_byte_for_byte`. |
| `native-builtins-security/src/sunec_intpoly.rs:107,121,132,146` | `NullPointerException` / `ArrayIndexOutOfBoundsException` for a null or wrong-kind limb array. Mis-typed relative to a hypothetical ideal, but **loud and catchable** — not a degradation to a success-shaped value, so out of this pass's remit. Changing the type risks perturbing an unrelated caller. |
| `native-builtins-security/src/sunec_point.rs:448` | The non-canonical-scalar ⇒ identity branch. Load-bearing for `ECDHKeyAgreement.validate`; changing it needs per-curve group orders and end-to-end ECDH validation. Marked `// TRUST BOUNDARY:` in source and listed below. |

**Count: 13 sites changed, 7 classes of site deliberately left** (each with the
reasoning above and a matching in-source comment).

---

## 3. Algorithms: supported vs explicitly rejected

### `native-builtins-crypto`

| Surface | Supported | Explicitly rejected |
|---|---|---|
| `signature::verify_rsa_pkcs1_v15_checked` | RSA PKCS#1 v1.5 with **SHA-1, SHA-256, SHA-384, SHA-512** | Everything else. `DigestAlgorithm` is a **closed enum with no `Unknown` variant**, so no OID can be coerced to a default. **MD2/MD5** (JDK-disabled for signatures) and **RSASSA-PSS** (different padding — `Pkcs1v15Sign` would be the wrong verifier) must never be added as aliases. Modulus **> 4096 bits** is rejected by the backend and now reported as `InvalidKeyException`. |
| `bc_aes` | AES-128/192/256 single-block, encrypt and decrypt (FIPS-197 KATs) | Every other key length — 0/1/8/15/17/20/23/25/31/33/64 bytes all raise. No pad, no truncate. |
| `bc_chacha` | ChaCha / Salsa20 cores and the SPHINCS-256 `Permute`, **even** round counts | Odd round counts raise with BouncyCastle's own message. |
| `bc_newhope` | NewHope N=1024, Q=12289 NTT + SHAKE128 `Poly.uniform` | No other parameter set exists in the API. |

### `native-builtins-security`

| Surface | Supported | Explicitly rejected |
|---|---|---|
| `sunec_point` (`ECOperations.multiply`) | **NIST P-256, P-384, P-521** only | Every other curve, *including ones the JDK itself ships* (secp256k1, P-192, Curve25519/X25519, Ed25519). Detected by field-implementation class name; no "closest match" fallback. Also rejected: off-curve points, wrong-length coordinates, scalars with significant bytes past the field width. |
| `sunec_intpoly` | P-256 Montgomery field `mult`/`square` only | The registration is class-scoped to `MontgomeryIntegerPolynomialP256`; no other field is reachable. |

### Not present in these crates at all

**TLS/SSL, `KeyStore`, `CertificateFactory`, `MessageDigest`, `KeyFactory`,
`KeyPairGenerator`, `SecureRandom`, and `Provider` lookup do not exist in
either crate** — verified by exhaustive grep over both source trees. Nothing
was invented for them here. They live in `native-builtins/` and are covered
under [Residual gaps](#residual-gaps).

---

## 4. Trust boundaries

Every site below carries a `// TRUST BOUNDARY:` comment in source.

### 4.1 Signature verification is not trust establishment

`native-builtins-crypto/src/signature.rs:53`

`verify_rsa_pkcs1_v15_checked` returning `true` means exactly one thing: *these
bytes are a valid PKCS#1 v1.5 signature over this message under this public
key.* It says nothing about whether the key is trusted, whether its certificate
chains to an anchor, whether that chain is in date, or whether it has been
revoked.

### 4.2 Signed JARs — **must not be used as a trust boundary**

`native-builtins-crypto/src/signature.rs:137`

> **Signed JARs must not be relied upon as a trust boundary until
> certification-path validation is complete.**

The shared verifier confirms the *cryptographic* signature. The consumer
(`classloading/src/jar_signer.rs`) performs no `PKIXValidator`-equivalent
certification-path construction, no trust-anchor check, no validity-window
check, and no revocation check. A JAR signed by *any* syntactically valid
self-generated key therefore passes signature verification. Treating "the JAR
is signed" as "the JAR is from a trusted publisher" is unsound in this build.

Additionally, `jar_signer.rs:1516` still calls the ambiguous `bool` wrapper and
maps `false` to `SigVerify::Bad`, so an *unusable signer key* is currently
reported as a *bad signature*. Both outcomes refuse the JAR — the behaviour is
safe today, the reporting is not precise. `classloading/` already has the right
shape for the fix: a distinct `SigVerify::Unsupported` variant that this call
site does not yet use.

### 4.3 Non-canonical EC scalars are reported as the identity

`native-builtins-security/src/sunec_point.rs:448`

A scalar `>= n` yields the neutral element rather than an error. This is
required by `ECDHKeyAgreement.validate`'s public-key order check (`n·P = O`),
and no legitimate keygen/sign/ECDH scalar (always in `[1, n)`) reaches it. The
conflation is nonetheless real: a future caller passing a `>= n` scalar for an
actual multiply would receive the identity, and identity is a *plausible*
answer. Narrowing it requires the group order per curve plus end-to-end ECDH
validation.

---

## 5. Residual gaps

Ordered by exposure. Items 1–3 are **outside** the two crates audited here.

1. **TLS is entirely in `native-builtins/` and was not audited.**
   `tls.rs` (4344 lines), `tls_impl.rs` (2774), `t27_tls.rs` (10248). One
   structural hazard is visible from the source comments and should be the
   first thing a follow-up pass looks at:
   `native-builtins/src/t27_tls.rs:4350` states that an
   `SSLServerSocketFactory.createServerSocket` overload without an explicit
   native bridge *"reaches `ServerSocketFactory`'s plaintext implementation"* —
   i.e. **a plaintext `ServerSocket` returned where an `SSLServerSocket` was
   requested**. The doc comment at `:4167`–`:4170` says the same thing from the
   other side ("so callers cannot accidentally fall through to
   `ServerSocketFactory`'s plaintext implementation"). Known-affected overloads
   were bridged individually as they were found; the structural point is that
   any *unbridged* overload still degrades silently to plaintext. This is the P1
   "unsupported TLS must be impossible to mistake for transport security" item
   and it needs an explicit deny-by-default at the factory, not per-overload
   patches.

2. **The JCA engine surface is in `native-builtins/` and was not audited.**
   `jca/signature.rs`, `jca/cipher.rs`, `jca/key_factory.rs`,
   `jca/message_digest.rs`, `jca/provider_chain.rs`, `crypto_impl.rs`,
   `keystore.rs`, `x509_manager.rs`, `securerandom.rs`. Two degradations are
   already visible in that tree without a full audit:
   `phases_early.rs:14701` returns `Ok(Some(Value::Object(None)))` — a **null**
   — from `Cipher.doFinal` on an uninitialised Cipher, explicitly *instead of*
   `IllegalStateException`; and `phases_early.rs:14706`/`:14714` read the
   algorithm name and key bytes with `unwrap_or_default()` / `Vec::new()`, so a
   missing key becomes an empty key rather than an `InvalidKeyException`. The
   crate does have the right mechanism (`throw_jca_exc`) and uses it correctly
   elsewhere (e.g. `jca/cipher.rs:637` for `AES/CCM`) — the gap is coverage,
   not capability.

3. **Certification-path validation for signed JARs** — see §4.2. Until it
   exists, signed JARs are not a trust boundary.

4. **Migrate the infallible kernels to their `try_*` forms.** Four AES call
   sites (`native-builtins/src/phases_late/bouncycastle.rs:6098`, `:6110`,
   `:6208`/`:6210`, `:6585`/`:6592`) and three ChaCha/Salsa call sites
   (`:7237`, `:7264`, `:7287`). The ChaCha ones are a pure consolidation —
   each already performs the identical parity check by hand. The AES ones are
   **not**: all four validate with `kw.len() < 2`, a lower bound that still
   admits a 2- or 4-entry schedule, which the round structure indexes past the
   end of. `try_encrypt_block`/`try_decrypt_block` check the length exactly
   (11/13/15). Only BouncyCastle's own `generateWorkingKey` currently feeds
   these call sites, so no live path reaches the hole — but the callers' guard
   is weaker than it reads.

5. **Migrate `classloading/src/jar_signer.rs:1516`** to
   `verify_rsa_pkcs1_v15_checked` and map `Err` to `SigVerify::Unsupported`
   (the variant already exists) rather than `SigVerify::Bad`.

6. **Narrow the non-canonical-scalar identity branch** — see §4.3.

7. **`sunec_point.rs` leaks GC pins on the error paths.** The unsupported-curve
   arms call `unpin_native_roots(pin_base)` before returning, but the `?`
   early-returns after it (`getX`/`getY`/`asBigInteger`/`read_bigint_be`, and
   the new scalar-read refusal is before any pin is taken) do not. This is a
   root-set leak, not a cryptographic degradation, and was left alone to keep
   this pass to failure semantics — but it should be fixed with a scope guard.

---

## 6. Test coverage for this contract

All tests are `#[cfg(test)]` inside the two crates.

| Property required | Test |
|---|---|
| Supported algorithm succeeds | `signature::tests::every_supported_digest_verifies_its_own_signature`, `bc_aes::tests::fips197_aes128` / `fips197_aes192_aes256`, `bc_chacha::tests::try_chacha_core_matches_rfc8439_block`, `sunec_point::tests::generator_times_{one,two}*` |
| Unsupported algorithm **raises**, not returns | `bc_aes::tests::supported_key_lengths_succeed_and_others_raise`, `bc_chacha::tests::odd_round_counts_raise_illegal_argument`, `sunec_point::tests::unsupported_curve_field_classes_are_declined` |
| Malformed key raises | `signature::tests::{empty_modulus,zero_modulus,empty_or_zero_exponent,even_exponent}_raises_invalid_key`, `oversized_but_legitimate_modulus_raises_invalid_key_not_false` |
| Invalid parameters raise | `signature::tests::{empty,wrong_length}_signature_raises_signature_exception`, `sunec_point::tests::{oversized_scalar_is_refused_not_truncated,wrong_length_coordinates_are_refused,off_curve_point_is_refused}` |
| Uninitialised object raises | `bc_aes::tests::malformed_key_schedule_raises_illegal_state` |
| **Genuine mismatch still returns `false`, no exception** | `signature::tests::genuine_mismatch_is_ok_false_not_an_error`, `corrupt_signature_bits_are_ok_false` |
| Guard does not perturb the transform | `bc_aes::tests::try_wrappers_match_the_infallible_pair`, `bc_chacha::tests::even_round_counts_succeed_and_match_the_infallible_kernels` |
| Guard does not false-positive | `sunec_point::tests::zero_padded_oversized_scalar_is_still_accepted`, `bc_aes::tests::every_real_key_size_produces_an_accepted_schedule` |
| Fail-closed legacy wrapper | `signature::tests::bool_wrapper_is_fail_closed_on_every_error_path` |
| Exception names are usable by the facade | `failure::tests::every_class_constant_is_internal_form`, `constructors_carry_the_jdk_specified_class` |
| Documented identity signal is stable | `sunec_point::tests::identity_results_are_signalled_with_empty_coordinates` |

**Provider-lookup miss** has no test here because no provider-lookup path
exists in either crate (§3); the constant `NO_SUCH_PROVIDER_EXCEPTION` is
provided for the facade and is covered by the internal-form test.
