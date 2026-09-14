# Signed-JAR trust boundary

**Scope:** `classloading/src/jar_signer.rs` and the signature/manifest paths of
`classloading/src/class_path.rs`.
**Companion:** [`crypto-failure-contract.md`](crypto-failure-contract.md) — the
same rule applied one layer down, in `native-builtins-crypto`.

---

## 0. Verdict

> **Signed JARs are not a trust boundary in this VM.** Do not use
> `Class.getCodeSource().getCertificates()` as an authorisation input.

The reason is *not*, as the P0 report assumed, that certification-path
validation is absent. It is largely present, and this audit deliberately does
**not** blanket-reject working functionality. The residual reasons are narrower
and are enumerated in [§4](#4-residual-gaps):

1. **No revocation checking at all.** No CRL, no OCSP. A signer certificate
   that its issuer revoked this morning still validates. This is the single
   biggest gap and it is unconditional.
2. **No name-constraint or certificate-policy processing.**
3. **Only the first `SignerInfo` of a signer block is examined** — a
   multi-signer JAR collapses to "the first signer verified".
4. **Anchor provenance is only as good as the host.** With no
   `JAVA_HOME/lib/security/cacerts`, no `javax.net.ssl.trustStore` and no
   `CRATONVM_TRUST_PEM`, the process trust store has **zero anchors** and every
   JAR is reported unsigned. That is fail-closed and safe, but it means the
   presence or absence of a signer says as much about host configuration as
   about the JAR.

What *is* complete is stated just as precisely, because refusing to say so
would be its own kind of dishonesty: the CMS walk, the `.SF` binding, the
SignerInfo public-key check, path construction to an anchor with per-link
signature verification, validity windows, the RFC 5280 BasicConstraints /
KeyUsage / ExtendedKeyUsage / unknown-critical rules, the manifest→entry digest
chain, and the per-entry gate that stops an entry absent from the manifest from
inheriting the signer's identity. Those checks are real and are worth keeping.

---

## 1. Audit table

Every API by which a caller could conclude "this JAR/entry is signed by a
trusted party". `T` = trust-establishing, `I` = integrity-only.

### 1.1 `classloading/src/jar_signer.rs`

| API | Kind | What it verifies | What it does **not** verify | Verdict |
|---|---|---|---|---|
| `verify_signer_block` `:357` | **T** | Outer `ContentInfo` is `pkcs7-signedData`; ≥1 `SignerInfo`; `contentType` authenticated attribute = `pkcs7-data`; `messageDigest` attribute = `H_alg(sf_bytes)` (constant-time); ≥1 embedded certificate; SignerInfo signature over the DER `SignedAttributes` verifies under the leaf SPKI; full [`verify_chain_path`] walk to an anchor. Input capped at 1 MiB. | Revocation. Name constraints / policies. Signers past the first. **Anything outside the signer block and `.SF`** — it never sees `MANIFEST.MF` or any archive entry. | **Fail-closed and honest.** `Some(_)` is necessary but not sufficient for trust; `chain` now carries only the validated path. |
| `parse_signed_data` `:428` | **T** (internal) | As above, minus the chain walk. `enforce_pubkey=false` only under `#[cfg(test)]` stores. | Same. | OK — the pubkey gate cannot be disabled in a production build (see `TrustStore::permissive_legacy_tests`). |
| `verify_chain` `:3041` | **T** | Delegates to `verify_chain_path`; identical decision. | — | OK. Retained for callers (incl. `fuzz_signed_jar`) that only need the yes/no. |
| `verify_chain_path` `:3074` | **T** | Name-chaining subject↔issuer; per-link signature (`link_signature_ok`); leaf + every parent + anchor validity window; leaf KeyUsage/EKU; each CA's BasicConstraints/`pathLenConstraint`/KeyUsage; unknown-critical-extension rejection; cycle detection; `MAX_CHAIN_LEN` = 16; terminates **only** at `find_anchor_by_subject`. | Revocation (§4.1). Name constraints, policy mapping, policy constraints (§4.2). `AuthorityKeyIdentifier`-based parent selection (DN match only). | **Genuinely complete for what it claims.** Returns the validated path so callers can report only what was checked. |
| `X509Cert::link_signature_ok` `:2663` | **T** (one link) | RSA PKCS#1 v1.5 SHA-1/256/384/512; ECDSA P-256/P-384 SHA-1/256/384/512; DSA SHA-1/256. Point-on-curve re-validated by the RustCrypto decoders. | That the parent is an anchor, is in date, or is entitled to be a CA. **One link only.** | OK. Three-valued: `BadSignature` ≠ `NotImplemented`. |
| `X509Cert::parse` `:2528` | — | Structural DER: outer/TBS/issuer/subject/SPKI/signature framing, no trailing bytes. | Semantic validity. Tolerates an unreadable `validity` field (leaves tag `0`) — which is why `cert_dates_ok` must fail closed. | OK as a parser. |
| `cert_dates_ok` `:2795` | **T** | `notBefore ≤ now ≤ notAfter`, UTCTime and GeneralizedTime. | Clock trust (wall clock, no monotonic/attested source). | **Changed:** an unreadable bound is now a refusal, not an implicit pass. |
| `verify_signature_with_spki` `:1726` | **T** (crypto only) | That `sig` is valid over `message` under the SPKI's key. | Whose key it is. | OK — labelled in-source as key-possession, not identity. |
| `rsa_pkcs1v15_verify` `:1665` | **T** (crypto only) | As above, for RSA. | As above. | **Changed:** migrated off the ambiguous `bool` (§2). |
| `TrustStore::{empty :2011, add_anchor_der :2048, load_pem_bundle :2094, load_default :2133}` | **T** (anchor ingestion) | Each anchor parses as X.509; `MAX_TRUST_ANCHORS` = 4096; JKS integrity MAC and PKCS#12 PFX MAC verified **before** any cert is extracted. | That an anchor *deserves* to be an anchor. Anchors are trusted by fiat — that is what an anchor is. Sources are host-supplied. | OK. `empty()` is maximally strict, not permissive. |
| `default_trust_store` `:2467` | **T** | Process-wide `OnceLock` over `load_default`. | — | OK, with the availability caveat in §0.4. |
| `verify_sf_binds_manifest` `:3337` | **I** | `H_alg(manifest_bytes)` equals the `.SF` main-section `<alg>-Digest-Manifest`; strongest algorithm wins; absent attribute ⇒ `false`. `*-Digest-Manifest-Main-Attributes` deliberately **not** accepted as a substitute. | Nothing about *who* wrote the `.SF`. No key, no certificate. | **Keep — renamed in docs as integrity.** Meaningful only after `verify_signer_block` succeeded on those same `.SF` bytes. |
| `parse_manifest_entry_digests` `:3404` | **I** (parser) | Nothing — it reports what the manifest says. Per-entry `Name:` + strongest `<alg>-Digest`; 4 KiB entry-name cap. | — | **Keep.** Security-relevant property is *omission*: an entry with no section, or a section with no recognised digest, yields no element. "Absent" ⇒ unsigned. |
| `digest_matches` `:3488` | **I** | `H_alg(data) == expected`, constant-time, length-checked first (an empty `expected` can never match). | Nothing about provenance. | **Keep.** |

### 1.2 `classloading/src/class_path.rs`

| API | Kind | What it verifies | What it does **not** verify | Verdict |
|---|---|---|---|---|
| `ClassPath::find_class_code_source_info` `:2747` | **T** (the Java-visible one) | Delegates: archive-level signer verification (memoized) + per-class entitlement. | Everything in §4. Also: an **empty** cert vector is ambiguous — unsigned JAR, failed verification, directory entry, and no-anchors-configured are indistinguishable. | OK, but this is the API most likely to be misread. Callers must fail closed on empty and must not treat non-empty as authorisation. |
| `ClassPath::extract_jar_signer_blocks` `:2911` | **T** | Per signer block: safe entry names (zip-slip screened), `.SF` companion located case-insensitively, `verify_signer_block`, then `verify_signed_entries` over the whole manifest. Certs contributed only if **both** pass. | Same residual set. | OK. Ordering (`verify_signer_block` *before* `verify_signed_entries`) is load-bearing and now documented as such. |
| `ClassPath::verify_signed_entries` `:3184` | **I**, conditional | `.SF` binds this exact `MANIFEST.MF`; every declared entry exists, is readable, and hashes to its declared digest; manifest `Name:` values screened by `is_safe_entry_name`. Any failure ⇒ `None` for the whole block. | Provenance — it is all digests. Depends entirely on the caller having verified the `.SF` first. | OK. |
| `ClassPath::certs_for_signed_class` `:3079` | **T** (per-entry gate) | The *served* entry name (resolving multi-release overrides first) is in `signed_entries`, and the bytes about to be loaded still hash to the signed digest. | Same residual set. | **Complete for the partial-signing gap.** Every non-success path returns an empty vector. |
| `JarSignerInfo` `:380` | — | Memoized `(chain, signed_entries)` per archive. | — | OK — the per-class decision stays per class even though the archive scan is cached. |

---

## 2. The three-valued result contract

The core fix. `jar_signer.rs:1665` called `verify_rsa_pkcs1_v15`, a `bool`
wrapper, and mapped `false` onto `SigVerify::Bad`.

A `bool` cannot express the difference between:

* **"we checked and the answer is no"** — the padded digest did not match.
  This is a *security decision*, and it is what a forgery looks like.
* **"we never checked"** — the key was rejected before any RSA operation ran.

The backend rejects an even exponent, `e < 2`, `e > 2³³−1`, and any modulus
over `RsaPublicKey::MAX_SIZE` (**4096 bits**). A JAR signed with an entirely
legitimate **8192-bit** key therefore came back `false` and was reported as
*"SignerInfo signature does not verify against signer public key"* — an
accusation of tampering levelled at a JAR nobody had examined.

### The contract

`SigVerify` (`jar_signer.rs:1424`) is the three-valued result. It is private to
the module; every call site matches all three arms explicitly, and there is no
`_ =>` arm that a future fourth state could default into.

| Variant | Meaning | Verification performed? | Trusted? |
|---|---|---|---|
| `Ok` | Valid signature under this key. | Yes — positive. | Still **no**: key possession ≠ identity. Trust is `verify_chain`'s. |
| `Bad` | The signature bytes do not match. | Yes — negative. **Preserved.** | No. |
| `Unsupported` | **Nothing was verified.** | **No.** | No. |

`Unsupported` now covers every "no verification happened" cause:

* algorithm OID not recognised (`sig_alg_digest` → `None`);
* algorithm recognised, key type/curve not carried (P-521, Brainpool, explicit
  `ECParameters`, DSA with a digest other than SHA-1/256, key/family mismatch);
* `SubjectPublicKeyInfo` will not parse (was `Bad`);
* the backend **rejected the key** — including the legitimate >4096-bit case
  (was `Bad`);
* the signature *encoding* is malformed — wrong length for the modulus, or an
  undecodable DER `SEQUENCE { r, s }` (was `Bad`).

Both `Bad` and `Unsupported` refuse. They are propagated as distinct outcomes
all the way out — `TrustError::BadSignature` vs `TrustError::NotImplemented`,
and two different `parse_signed_data` messages — so no caller can conflate
them, and a diagnostic never claims a verdict that was not reached.

### Sites changed

| Site | Was | Now |
|---|---|---|
| `rsa_pkcs1v15_verify` `:1665` | `verify_rsa_pkcs1_v15(..) -> bool`; `false ⇒ Bad` | `verify_rsa_pkcs1_v15_checked`; `Ok(true) ⇒ Ok`, `Ok(false) ⇒ Bad`, `Err ⇒ Unsupported` + `warn!` |
| `verify_signature_with_spki` `:1726` | `parse_spki` `Err ⇒ Bad` | `Err ⇒ Unsupported` + `warn!` |
| `ecdsa_p256_verify` / `ecdsa_p384_verify` | undecodable key or signature ⇒ `Bad` | ⇒ `Unsupported` |
| `dsa_verify` | undecodable key or signature ⇒ `Bad` | ⇒ `Unsupported` |
| `parse_signed_data` `:428` | `Unsupported` ⇒ *"signature algorithm not verifiable (unsupported curve/key)"* | *"signature could not be verified (unsupported or unusable algorithm/key) — refusing"* |

**Did the migration change any accept/reject decision?** No. Every affected
path already refused, and still refuses. What changed is that the refusal now
names the right reason, and an over-large-but-legitimate signer key is
diagnosable instead of looking like a tampered JAR. This is the "safe today,
imprecise today" item closed from
[`crypto-failure-contract.md`](crypto-failure-contract.md) §4.2 / gap 5.

---

## 3. Other trust-boundary changes in this pass

### 3.1 `chain` is now the validated path, not the CMS bag

`verify_chain` returned `Result<(), TrustError>`, so `verify_signer_block` had
nothing to narrow with and passed the **entire** CMS `certificates` set out as
`VerifiedSigner.chain`. `class_path::extract_jar_signer_blocks` pushed every
element of that into the `CodeSource` certificate array.

The CMS `certificates` field is attacker-supplied. An attacker could append a
certificate of their choosing to an otherwise legitimately signed JAR: path
construction ignores it (correctly), but it was still surfaced through
`Class.getCodeSource().getCertificates()` as one of the signer's certificates —
enough to fool a policy `signedBy` filter that scans the array rather than
looking only at element 0.

`verify_chain_path` (`:3074`) now returns the certificates that actually formed
the path, leaf first and anchor last, appending each only *after* its link
signature, validity window and extension checks have passed. `verify_chain`
delegates to it and is behaviourally identical, so the `fuzz_signed_jar`
oracle (zero-anchor store ⇒ `verify_signer_block` returns `None`,
`verify_chain` returns `Err`) is untouched. `verify_signer_block` narrows
`vs.chain` to that path.

Covered by `tb_unvalidated_certificates_are_not_surfaced_as_the_signers`.

### 3.2 `cert_dates_ok` fails closed on an unreadable validity window

It previously returned `true` when `parse_asn1_time` could not read a bound —
"conservatively treat the cert as valid on the date axis". That is the same
did-not-check-reads-as-checked conflation as the `bool` above, one level up: it
recorded *in date* for a certificate whose dates were never examined.

RFC 5280 §4.1.2.5 admits exactly two encodings and `parse_asn1_time` accepts
both, so every conforming certificate still parses. What is now rejected is a
certificate with a missing, truncated, or non-`Time` validity field —
structurally tolerated by `X509Cert::parse`, emitted by no CA.

Covered by `tb_unreadable_validity_window_rejects`.

---

## 4. Residual gaps

Ordered by exposure. Until items 1 and 2 exist, §0's verdict stands.

1. **Revocation (CRL / OCSP) is not implemented.** RFC 5280 §6.3 is absent
   entirely. A certificate that chains and is in-validity is accepted even if
   its issuer revoked it. This matches stock HotSpot `jarsigner` absent an
   explicit `-revCheck`, and revocation needs network I/O the class loader
   deliberately avoids — but the consequence is unchanged: **a compromised
   code-signing key cannot be un-trusted** short of removing its anchor.
   Closing it needs a CRL/OCSP fetcher, a cache with a freshness policy, and a
   decision about soft-fail vs hard-fail (soft-fail would reintroduce exactly
   the "we did not check" ambiguity this document is about, so it must be
   hard-fail or configurable-and-loud).

2. **NameConstraints, certificate policies, policy mapping, policy
   constraints.** Not implemented. Partly mitigated: `nameConstraints` is
   marked *critical* by CAs, and an unrecognised critical extension is rejected
   outright (`extract_ext_facts` `:2881`, RFC 5280 §6.1.4(f)), so an
   unprocessed one cannot silently broaden trust. A *non-critical*
   `nameConstraints` is ignored.

3. **Only the first `SignerInfo` is verified.** A multi-signer JAR reports
   "first signer verified"; the others are neither verified nor reported.

4. **Parent selection is by Subject/Issuer DN only.** `AuthorityKeyIdentifier`
   is parsed as a recognised extension but not used to disambiguate, so with
   two certificates sharing a Subject DN the first in slice order wins. Both
   still have to pass the signature check, so this cannot create a forged path
   — it can only fail to find a valid one.

5. **Anchor provenance and clock.** Anchors come from host files (`cacerts`,
   `javax.net.ssl.trustStore`, `CRATONVM_TRUST_PEM`) or the
   `extend_from_anchors` seam. Validity windows are compared against
   `SystemTime::now()` — an attacker who controls the clock controls expiry.

6. **`native-builtins/src/crypto_impl.rs:2225` still calls the `bool`
   wrapper.** Out of scope for this pass (that tree was not editable here). It
   is fail-closed, but it carries the same ambiguity this document removed from
   the class loader, and it should be migrated to
   `verify_rsa_pkcs1_v15_checked`.

7. **DSA digests beyond SHA-1/SHA-256, and EC curves beyond P-256/P-384.**
   Recognised and rejected as `Unsupported` — fail-closed, and correctly
   *reported* as unsupported since this pass.

8. **`OID_STUB_SIG` remains a `#[cfg(test)]` backdoor.** `link_signature_ok`
   and `is_stub_sig_fixture` compile it out of production builds, and
   `production_trust_store_constructors_are_never_permissive` guards the
   related `permissive_legacy` switch. No action needed; listed so it is not
   rediscovered as a finding.

### What would have to be built, in order

1. Revocation: CRL distribution-point / OCSP responder fetch, a bounded cache,
   and an explicit hard-fail-or-configurable policy. (Gap 1.)
2. Full RFC 5280 §6.1 state machine: name constraints, `certificatePolicies`,
   policy mapping and policy constraints, with the initial-policy-set inputs.
   (Gap 2.)
3. Multi-`SignerInfo` dispatch, and a decision about what "signed by" means
   when signers disagree. (Gap 3.)
4. AKI/SKI-driven parent selection and multi-path search on ambiguity.
   (Gap 4.)
5. A trusted time source for validity evaluation, or signature timestamps
   (RFC 3161 `signatureTimeStampToken`), which would also make expired-signer
   JARs verifiable the way HotSpot does. (Gap 5.)

Only after 1 and 2 should any caller be allowed to treat
`getCodeSource().getCertificates()` as an authorisation input, and even then
the per-entry gate in §1.2 remains mandatory.

---

## 5. Test coverage for this contract

All `#[cfg(test)]` in `classloading/src/jar_signer.rs`. The fixtures use the
real RSA test key and the production verification path, not the `OID_STUB_SIG`
backdoor.

| Property required | Test |
|---|---|
| Unsupported algorithm is `Unsupported`, never `Bad` | `tb_unsupported_signature_algorithm_is_unsupported_not_bad` (PSS OID, wrong key family, unknown OID, unparseable SPKI) |
| ...and is rejected by the trust API, with a diagnosis that does not claim a failed verification | `tb_unsupported_algorithm_is_rejected_at_the_trust_api` |
| A malformed signature *encoding* is `Unsupported`, not `Bad` | `rsa_verify_rejects_structural_garbage` (updated) |
| **Genuine mismatch stays `Bad`** (preserved negative) | `tb_genuine_signature_mismatch_is_bad_not_unsupported`, `rsa_verify_accepts_valid_signature`, `rsa_verify_rejects_structural_garbage` (`s == n` case) |
| The two stay distinct at the `TrustError` level | `tb_unsupported_and_bad_stay_distinct_trust_errors` |
| A valid self-consistent signature with no anchor establishes no trust | `tb_self_consistent_signature_without_an_anchor_is_not_trusted`, `task40_self_signed_chain_rejected_without_anchor`, and the `fuzz_signed_jar` zero-anchor oracle |
| Certificates that took no part in the path are not surfaced | `tb_unvalidated_certificates_are_not_surfaced_as_the_signers` |
| Expired / not-yet-valid certificates reject | `tb_expired_and_not_yet_valid_certs_reject`, `validity_dates_reject_expired_cert` |
| An unreadable validity window rejects | `tb_unreadable_validity_window_rejects` |
| An entry absent from the manifest is not covered | `tb_entry_absent_from_manifest_is_not_covered`, `parse_manifest_entry_digests_extracts_each_section` |
| **Integrity checking still works** (the preserved capability) | `tb_integrity_checking_still_works`, `sf_binds_manifest_accepts_matching_digest`, `parse_manifest_entry_digests_picks_strongest_alg`, `end_to_end_real_rsa_signer_block_and_chain` |
| `.SF` binding fails closed without `-Digest-Manifest` | `sf_binds_manifest_fails_closed_without_digest_manifest` |
| RFC 5280 extension rules | `rfc5280_leaf_keyusage_without_digitalsignature_rejected`, `rfc5280_leaf_with_digitalsignature_and_codesigning_eku_accepted`, `rfc5280_intermediate_with_ca_false_rejected`, `rfc5280_unknown_critical_extension_rejected` |
| The `permissive_legacy` fail-open cannot be reached in production | `production_trust_store_constructors_are_never_permissive` |
| Trust-store ingestion is bounded and MAC-checked | `jks_round_trip_extracts_trusted_cert`, `jks_wrong_password_rejected`, `jks_tampered_body_rejected`, `task40_pem_bundle_decode_round_trip` |

`fuzz/fuzz_targets/fuzz_signed_jar.rs` is unchanged and its load-bearing
assertion — `verify_signer_block` against a zero-anchor `TrustStore` never
returns `Some` — still holds: `verify_chain` delegates to `verify_chain_path`,
which still terminates successfully only at `find_anchor_by_subject`.

---

## 6. Compatibility impact

| Change | Previously | Now | Who is affected |
|---|---|---|---|
| `bool` → three-valued result | Refused, reported as a bad signature | Refused, reported as unverifiable | Nobody's accept/reject changes. Log text and `TrustError` variant change. |
| `chain` narrowed to the validated path | Whole CMS `certificates` set | Only the validated path | A JAR whose signer block carries certificates outside the path now reports fewer certificates. `chain[0]` (the leaf) is unchanged, so any caller reading the leaf is unaffected; a caller scanning the whole array sees only verified entries. |
| `cert_dates_ok` fails closed | A certificate with an unreadable `validity` was treated as in-date | Rejected (`TrustError::Expired`) | Only certificates whose `validity` is not a conforming `UTCTime`/`GeneralizedTime` — non-conforming per RFC 5280 §4.1.2.5, and not emitted by any CA. No conforming certificate changes behaviour. |
| Undecodable SPKI / key / signature encoding | `SigVerify::Bad` → `TrustError::BadSignature` | `SigVerify::Unsupported` → `TrustError::NotImplemented` | Both refuse. A caller that matches specifically on `BadSignature` would now see `NotImplemented` for these inputs; no such caller exists in-tree. |

No public item was removed or renamed. `verify_chain_path` is added;
`verify_chain` keeps its signature and its behaviour.
