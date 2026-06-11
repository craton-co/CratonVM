# Fix note — nb-sec-crypto

Addresses findings **B1**, **B2**, **V1** from `docs/reviews/fable-2026-06-10/nb-security.md`.

All edits are confined to `native-builtins/src/crypto_impl.rs` (the only file the
findings touched). `x509_manager.rs` and `jca/` were inspected but needed no change
(see "Cross-checks" below).

---

## B1 (HIGH DoS) — RSA PKCS#1 v1.5 encode underflows on small keys

### Finding
`Rsa::pkcs1v15_encode` computed `ps_len = k - t_len - 3` with an unguarded `usize`
subtraction. `k` derives from the issuer public key parsed out of an
attacker-supplied certificate; a tiny RSA modulus (e.g. 256-bit → `k=32`, while
SHA-256 needs `t_len=51`) makes `32 - 51 - 3` underflow → panic (debug) or wrap to
~`usize::MAX` then `repeat(0xff).take(ps_len)` multi-exabyte allocation → abort
(release). Reachable via `verify_signature → Rsa::verify_sha256 → pkcs1v15_encode`,
driven by `checkServerTrusted`/`PKIXValidator.engineValidate`.

### Root cause
No RFC 8017 §9.2 minimum-size check (`k >= tLen + 11`) before computing the
padding length.

### Exact change (`crypto_impl.rs`)
- `pkcs1v15_encode` (≈1738): return type changed `Vec<u8>` → `Option<Vec<u8>>`;
  the padding length is now `k.checked_sub(t_len + 3).filter(|&ps| ps >= 8)?`,
  i.e. it returns `None` when the key is below the minimum (`k < tLen + 11`).
- `verify_sha256` (≈1707): `let Some(expected) = Self::pkcs1v15_encode(...) else { return false; };`
  — a sub-minimum modulus is now a **verification failure** (fail-closed), which
  `verify_signature` already propagates and `x509_manager::verify_one_signature`
  turns into a `CertificateException`.
- `sign_sha256` (≈1696): `let Some(em) = ... else { return Vec::new(); };`
  — signing with a too-small key yields an empty signature (a failure for callers);
  a real RSA signing key is always large enough so this path is unreachable in
  practice.

Both call sites are in-file; no external signature change.

---

## B2 (HIGH DoS) — DER length-field integer overflow → slice panic

### Finding
`der_read_length` accepted up to 8 length octets and accumulated `len = (len<<8)|byte`
with no cap; `der_read_tag_length`/`der_read_integer` then computed
`total_hdr + len` (could wrap in release), passed the guard, and sliced
`&data[total_hdr..total_hdr+len]` → inverted/OOB range → panic. Reachable from any
DER fed to `X509Cert::parse_der` and the cert/keystore loaders.

### Root cause
Unchecked length accumulation + unchecked `total_hdr + len` add before slicing.

### Exact change (`crypto_impl.rs`)
- `der_read_length` (≈2412): reject an over-wide width up front
  (`if num_bytes > core::mem::size_of::<usize>() { return None; }`). With the width
  bounded to ≤ 8 the `(len<<8)|byte` accumulation fits a `usize` exactly and cannot
  overflow; any oversized-but-in-range `len` is caught downstream.
- `der_read_tag_length` (≈2398): `let end = total_hdr.checked_add(len)?;`
  then `if data.len() < end { return None; }` and slice `&data[total_hdr..end]`.
  A forged `len` near `usize::MAX` overflows `checked_add` → `None`.
- `der_read_integer` (≈2434): same `checked_add` + buffer-length guard.

This mirrors the already-hardened production parser
`x509_manager.rs::read_length`/`read_tlv` (which the report flagged separately and
which was already safe).

---

## V1 (HIGH latent) — `verify_cert_chain` trusts intermediates/anchors by CN string

### Finding
`verify_cert_chain` matched issuers/anchors via `subject_cn_matches` (CN **string**
equality only), enabling chain-confusion. Test-only / not wired to the live TLS path,
so latent — but must not become a silent trust bypass.

### Root cause
CN-string equality is not a safe trust predicate (two distinct issuers can share a CN).

### Exact change (`crypto_impl.rs`)
- Added `X509Cert::subject_der_matches(&self, issuer_der: &[u8]) -> bool` (≈2604):
  full Name-DER equality on `subject_raw` (empty Name never matches), mirroring
  `x509_manager::validate_chain`'s `issuer_der == subject_der` continuity check.
- `verify_cert_chain` (≈2602): both the next-in-chain issuer pick and the trust-anchor
  lookup now use `subject_der_matches(&cert.issuer_raw)` instead of
  `subject_cn_matches(&cert.issuer_cn)`. The next-in-chain element is additionally
  required to satisfy strict Name-DER continuity (`.filter(...)`).
- Added a prominent `# WARNING — not the production trust path` doc block on
  `verify_cert_chain` and a "Do NOT use this for trust decisions" note on the
  retained-but-diagnostic-only `subject_cn_matches`.

`subject_cn_matches` is kept (it is `pub` API and still exercised by
`rf10_subject_cn_matches_basic`), now clearly fenced as diagnostic-only.

---

## Tests added (`crypto_impl.rs`, `mod tests`)
- `b1_small_rsa_modulus_verify_rejects_without_panic` — 256-bit modulus + 32-byte
  signature → `verify_sha256` returns `false`, no panic.
- `b1_small_rsa_modulus_sign_returns_empty` — sub-minimum key → empty signature.
- `b2_der_oversized_length_rejected_without_panic` — 8×0xFF length (~usize::MAX) → `None`.
- `b2_der_overwide_length_field_rejected` — 9 length octets → `None`.
- `b2_truncated_der_is_parse_error` — declared length 100, 3 bytes present → `None`.
- `v1_subject_der_matches_uses_full_name` — full-DER match accepts equal DER, rejects
  near-miss and empty Name.

All use existing in-module helpers (`mock_cert`, `BigUint::from_bytes_be`, `Rsa`,
`RsaPublicKey`/`RsaPrivateKey` with `pub` fields, `der_read_tag_length`) and
`use super::*`, so they compile against the existing test patterns. Pre-existing
RF10 tests are unaffected: the mock sets `subject_raw`/`issuer_raw` from the same
strings as the CN fields, so DER-based matching gives identical results.

## Files touched
- `native-builtins/src/crypto_impl.rs`
- `docs/reviews/fable-2026-06-10/fixes/nb-sec-crypto.md` (this note)

## Cross-checks
- `x509_manager.rs::read_length`/`read_tlv` already cap width (`n > 8`) and use
  `checked_shl`/`checked_add` + buffer bound — no change needed; the production trust
  path was already safe. B2 was only the `crypto_impl.rs` parser.
- The production trust validator (`x509_manager::validate_chain`) was already DER-based
  and is the one wired to TLS; V1's `verify_cert_chain` stays test-only and is now
  both DER-safe and loudly fenced.

## Follow-up & risk
- Risk: **low**. `pkcs1v15_encode`'s return-type change is internal (two in-file
  callers updated). The DER guards only convert previously-panicking/overflowing
  inputs into clean `None`/parse errors — valid certs are unaffected (a legitimate
  content length always fits the buffer and ≤ 8 octets).
- Could not build/test here (sandbox rule). Confidence the edits compile is high:
  conservative, mirrors existing patterns, `Option` plumbing is local.
- Follow-up (out of scope for this task, other owners): B3/B4/B5 HTTP DoS in
  `http_client.rs`; S1 fail-closed PQC keygen in `jca/key_factory.rs`; S2 HPACK
  Huffman; the report's suggestion to fully delete `verify_cert_chain` and unify on
  `validate_chain` (I fenced rather than deleted to preserve its `pub` API + tests).
