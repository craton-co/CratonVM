# Crypto failure-mode audit — `native-builtins-crypto`, `native-builtins-security`

**Scope of the changes:** `native-builtins-crypto/` and
`native-builtins-security/` only. Everything else in this document is
**verified by reading and reported**, not changed — the signed-JAR and
certification-path code lives in `classloading/`, and the JCA/TLS engine
surface lives in `native-builtins/`, neither of which this pass could edit.
Cross-file requests are collected in [§6](#6-changes-needed-outside-these-two-crates).

**Companions:** [`../security/crypto-failure-contract.md`](../security/crypto-failure-contract.md)
(the rule and the earlier pass over these crates) and
[`../security/signed-jar-trust.md`](../security/signed-jar-trust.md) (the
trust boundary one layer up).

---

## 0. The defect shape

> A security operation that cannot do its job returns **a working, insecure
> result**.

The archetype on this branch was `SSLServerSocketFactory.createServerSocket()`
inheriting `ServerSocketFactory`'s cleartext body: a plaintext socket handed to
a caller who believed it had TLS. Nothing failed, nothing logged.

The tells, in order of how often they turned out to be real here:

| Tell | Found in these two crates? |
|---|---|
| A `-> ()` kernel that emits *something* for input it cannot honour | **Yes — 2 families, both fixed** (§1.1, §1.2) |
| A value that is simultaneously "the answer" and "I could not compute one" | **Yes — 1, fixed** (§1.3) |
| `unwrap_or_default()` / `unwrap_or(true)` / `.ok()` on a verification result | No — the previous pass removed the last one; re-swept and clean |
| Empty "problems found" list read by the caller as "verified" | No such shape exists in either crate |
| An optimistic stub standing in for a real JDK class | **No — neither crate registers a synthetic stub** (§4) |
| A default substituted for an unreadable input | **Yes — but the site is in `native-builtins/`** (§6.1) |

### Premise checks

The task brief flagged three claims to verify before implementing. Two were
already false:

* **"Certification-path validation is absent."** False, and the previous wave
  had already established it. Per-link signature verification, anchor
  matching, validity windows, BasicConstraints/`pathLenConstraint`, KeyUsage,
  ExtendedKeyUsage, unknown-critical rejection, cycle detection and a
  `MAX_CHAIN_LEN` are all present in `classloading/src/jar_signer.rs`
  (`verify_chain_path` `:3074`, `extract_ext_facts` `:2882`).
* **"A silent skip of name-constraint checking."** False for any conforming
  certificate. `nameConstraints` is *not* in the recognised-extension set at
  `jar_signer.rs:2901`, and every unrecognised **critical** extension is a hard
  refusal (`:2911`). RFC 5280 §4.2.1.10 requires `nameConstraints` to be
  critical, so the CA-compromise vector is closed by the general rule rather
  than by a specific check. The residue is narrow and real: a **non-critical**
  `nameConstraints` is silently ignored (§6.3).
* **"Revocation is missing."** True and unconditional. No CRL, no OCSP.

---

## 1. Entry-point table

Every entry point in the two crates. "Cannot do its job" is the question that
matters: *what does this hand back when it has not actually done the thing?*

### 1.1 `native-builtins-crypto/src/bc_chacha.rs` — **changed**

| Operation | Cannot-do-its-job case | Returned **before** | Returned **now** | Action |
|---|---|---|---|---|
| `chacha_core` / `salsa_core` `:249`, `:271` | `rounds == 0` | `x[i] = 2 * input[i]` — the engine state, doubled. **The engine state is the key.** | `panic!` naming BouncyCastle's own message | **FIXED** |
| `chacha_core` / `salsa_core` | `rounds` odd | keystream one round short, silently | `panic!` | **FIXED** |
| `permute` `:343` | `rounds <= 0` | the **identity function** — the SPHINCS hash becomes forgeable | `panic!` | **FIXED** |
| `try_chacha_core` / `try_salsa_core` / `try_permute` | odd rounds | `Err(IllegalArgumentException)` | unchanged | — |
| ...same | `rounds <= 0` | **`Ok(())`** — `0 % 2 == 0` passed the parity-only guard | `Err(IllegalArgumentException)`, BC's wording | **FIXED** |
| `chacha_permute_bytes` `:368`, `sphincs_hash_n_n`, `sphincs_hash_2n_n` | — | round count is the compile-time constant `12`; no input can vary it | unchanged | OK |

**Why this is the headline finding.** The previous pass added a round guard,
but only the *parity* half. BouncyCastle's own `Salsa20Engine` constructor
rejects `rounds <= 0 || (rounds & 1) != 0`; ours rejected only the second
clause. Zero passes a parity check, runs zero permutation rounds, and makes
`chacha_core` publish `2 * state`. XOR that against plaintext and the key falls
out of the ciphertext.

Zero is not a hypothetical value. `native-builtins/src/phases_late/bouncycastle.rs:6994`
reads the round count out of the Java object *by name*:

```rust
let rounds = match ctx.get_field_by_name(this, "rounds") {
    Value::Int(v) => v,
    _ => 20,
};
```

and a by-name read of an unwritten `int` slot yields `Value::Int(0)`, which
that `match` accepts as a genuine round count and passes straight through to
the kernel (`bc_stream_generate_key_stream` `:6890`/`:6894` validate nothing).
The guard added here is the backstop; the read itself still needs fixing and is
filed as §6.1.

The refusal is an abort because these three kernels return `()`. A `-> ()`
cipher has exactly three options for input it must not honour: emit the wrong
keystream (silent, and here it leaks the key), leave the output buffer at
whatever it held (silent, and the caller then XORs plaintext against a stale or
zero stream — plaintext in the clear), or abort. Only the third is a refusal.

### 1.2 `native-builtins-crypto/src/bc_aes.rs` — **changed**

| Operation | Cannot-do-its-job case | Before | Now | Action |
|---|---|---|---|---|
| `encrypt_block` / `decrypt_block` `:262`, `:334` | schedule length ∉ {11,13,15} | abort, but *incidentally* — via a `kw.len() - 1` underflow or whichever `kw[r]` index ran off the end first | abort **up front and by name**, before any table lookup | **HARDENED** |
| `encrypt_block` / `decrypt_block` | block shorter than 16 bytes | abort inside `le_to_u32` | abort by name | **HARDENED** |
| `try_encrypt_block` / `try_decrypt_block` `:213`, `:220` | both above | `Err(IllegalStateException / IllegalArgumentException)` | unchanged | OK |
| `generate_working_key` `:459` | key length ∉ {16,24,32} | `None` — not a usable schedule; no pad, no truncate | unchanged | OK |
| `try_generate_working_key` `:439` | same | `Err(IllegalArgumentException)`, BC's wording | unchanged | OK |

No accept/reject decision changes. What changes is that a malformed schedule
now aborts deterministically with a message that names the defect, instead of
depending on which index happens to fail first. This matters because all six
in-tree callers guard with a `kw.len() >= 2` **lower bound**
(`bouncycastle.rs:6081`, `:6199`, `:6565`, `:7578`, `:8886`, `:9091`) — a bound
that still admits a 2- or 4-entry schedule the round structure indexes past.

### 1.3 `native-builtins-security/src/sunec_point.rs` — **changed**

| Operation | Cannot-do-its-job case | Before | Now | Action |
|---|---|---|---|---|
| `scalar_mul_p{256,384,521}` | scalar non-canonical (`>= n`) | **the identity point** — for *every* such scalar | identity **only** for `s == n`; every other `s >= n` is `None` ⇒ thrown exception | **FIXED** |
| ...same | scalar has significant bytes past the field width | `None` ⇒ thrown | unchanged | OK |
| ...same | point not on the curve | `None` ⇒ thrown (closes the invalid-curve attack) | unchanged | OK |
| ...same | wrong-length coordinates | `None` ⇒ thrown | unchanged | OK |
| `native_ec_multiply` `:219` | scalar `byte[]` element is not an `Int` | thrown `IllegalArgumentException` | unchanged | OK |
| `native_ec_multiply` | field class unresolvable | thrown `ProviderException` | unchanged | OK |
| `native_ec_multiply` | unsupported curve | thrown `ProviderException`, naming the curve | unchanged | OK |
| `native_ec_multiply` | **any `?` early return** | GC pins **leaked** into the root set for the life of the VM | single `unpin` on every exit path | **FIXED** |

The non-canonical branch was the last "plausible answer for a question we could
not answer" in either crate, and it sat on a **private-key operand**. Exactly
one non-canonical scalar has a defined expected result: `s == n`, which
`ECDHKeyAgreement.validate` uses for its `n·P = O` public-key order check.
Every other `s >= n` now refuses. Keygen, signing and real ECDH scalars are
always in `[1, n)` and take the canonical branch, so the narrowing cannot
reject a legitimate multiply.

The group orders are hard-coded (no new dependency on a per-curve
`crypto-bigint` width) and **proved exact by test** rather than trusted:
`Scalar::from_repr(v)` is `Some` iff `v < n`, so `from_repr(ORDER) == None`
gives `ORDER >= n` and `from_repr(ORDER - 1) == Some` gives `ORDER <= n`.
Together, `ORDER == n`. A single mistyped digit fails one direction or the
other.

### 1.4 Inspected, unchanged

| Site | Why it stays |
|---|---|
| `signature.rs:62` `verify_rsa_pkcs1_v15_checked` | Already correct, including the **preserved negative**: a genuine digest mismatch stays `Ok(false)` and must not become an exception. |
| `signature.rs:161` `verify_rsa_pkcs1_v15` (`-> bool`) | Now `#[deprecated]` — see §2. |
| `bc_newhope.rs:150` `uniform` | A short or empty `seed` produces a well-formed `a` polynomial from the wrong bytes, and the caller (`bouncycastle.rs:7477`) leaves `seed` zeroed if the array read fails. Left alone deliberately: `a` is a **public** parameter in NewHope, BouncyCastle's own `Poly.uniform` performs no length check either, and adding one risks a false positive on the only caller. Recorded here so it is not rediscovered as a finding. |
| `bc_newhope.rs` `to_ntt` / `from_ntt` | Fixed-size `[i16; 1024]`, no algorithm selection, no key parsing. Nothing to degrade. |
| `sunec_intpoly.rs:91` `unwrap_or(0)` | Not a swallowed error: `BigUint` stores zero as an empty digit vector, so `None` occurs for exactly one value and `0` *is* that value. |
| `sunec_intpoly.rs:122,146` `read_limbs`/`write_limbs` | Loud `ArrayIndexOutOfBounds`/`NullPointerException` for a wrong-kind or short array. Mis-typed against an ideal, but catchable and never success-shaped. |
| `sunec_point.rs` `read_bigint_be:163` | Rejects a magnitude wider than the field. A *negative* `BigInteger` would be read as a positive magnitude, but the only inputs are field coordinates, which are non-negative by construction. |

---

## 2. The ambiguous `bool` verifier is now deprecated

`verify_rsa_pkcs1_v15` collapses "the signature does not match" and "the key was
rejected, so nothing was verified" into one `false`. It is fail-closed — `Err`
can never surface as `true`, and `bool_wrapper_is_fail_closed_on_every_error_path`
pins that — but ambiguity on a trust path is the defect this lane exists to
remove.

**It now has zero callers in the tree**, verified by grep over every `.rs` file
in the workspace; the remaining occurrences of the name are doc comments
describing the migration. Both former call sites are gone:

| Former caller | Now |
|---|---|
| `classloading/src/jar_signer.rs` `rsa_pkcs1v15_verify` | `verify_rsa_pkcs1_v15_checked`; `Err ⇒ SigVerify::Unsupported` (not `Bad`) |
| `native-builtins/src/crypto_impl.rs` `Rsa::try_verify_sha256` → `rsa_verify` | same; `None` means "never checked" |

Marked `#[deprecated]` rather than deleted so a mid-flight or out-of-tree
consumer does not break, and so anyone reaching for it is told at compile time
which function to use. Deletion is filed as §6.7.

---

## 3. Signed-JAR failure matrix

All of this is in `classloading/src/jar_signer.rs` and
`classloading/src/class_path.rs` — **read and verified, not changed** (out of
this pass's editable scope).

| Condition | Outcome | Distinct? | Silent pass possible? |
|---|---|---|---|
| Signature does not verify | `SigVerify::Bad` → `TrustError::BadSignature` → `parse_signed_data` returns *"SignerInfo signature does not verify against signer public key"* (`:602`) | Yes | No |
| Signer key rejected by the backend (e.g. a legitimate 8192-bit modulus) | `SigVerify::Unsupported` → `TrustError::NotImplemented` → *"signature could not be verified (unsupported or unusable algorithm/key) — refusing"* | Yes — **distinct from the above**, which is the point of the three-valued result | No |
| Signature encoding malformed (wrong length, undecodable DER `{r,s}`) | `Unsupported` | Yes | No |
| Signature algorithm OID unrecognised | `Unsupported` | Yes | No |
| **Missing signature** — no `SignerInfo` in the block | `Err("SignedData has no SignerInfo")` (`:481`) | Yes | No |
| **Missing `authenticatedAttributes`** | `Err("SignerInfo is missing authenticatedAttributes — refusing to skip integrity check")` (`:515`) | Yes | No |
| `messageDigest` attribute ≠ `H(.SF)` | refusal; constant-time compare | Yes | No |
| Unparseable manifest / `.SF` | `verify_sf_binds_manifest` `:3337` returns `false` on an absent or non-matching `-Digest-Manifest`; `verify_signed_entries` returns `None` for the **whole block** on any failure | Yes | No |
| **Digest mismatch on one entry** | whole block ⇒ `None`; and independently `certs_for_signed_class` `:3079` re-hashes the *served* bytes and returns an empty cert vector | Yes | No |
| Entry absent from the manifest | not covered — empty cert vector, so it cannot inherit the signer's identity | Yes | No |
| No trust anchors configured | every JAR reports unsigned — fail-closed, but indistinguishable from "unsigned" at the API | **No** | No (but see below) |
| **Multiple `SignerInfo`s** | **the first is verified; the rest are neither verified nor reported nor counted** (`:478`) | **No** | **Yes, in effect** — the caller is told "signed", with no indication that other signers exist |
| Revoked signer certificate | accepted | — | **Yes** |

Two rows are the residue, and both are outside these crates:

* **Multi-`SignerInfo`** is the one genuine *silent* degradation in the
  signed-JAR path. `signer_infos.into_iter().next()` takes the first and
  discards the rest without a count, a warning, or a marker on
  `VerifiedSigner`. Filed as §6.2.
* **Empty cert vector is overloaded** — unsigned JAR, failed verification,
  directory entry, and zero-anchors-configured are indistinguishable at
  `find_class_code_source_info` `:2747`. Fail-closed, but a caller cannot tell
  "not signed" from "we could not check". Filed as §6.4.

---

## 4. Certification-path gap decisions

| Gap | Real state (verified by reading) | Decision |
|---|---|---|
| **Revocation (CRL/OCSP)** | Absent entirely. RFC 5280 §6.3 is not implemented. A certificate that chains and is in-validity is accepted even if revoked this morning. | **Cannot implement here** — it needs network I/O the class loader deliberately avoids, a bounded cache with a freshness policy, and a hard-fail-vs-configurable decision. Soft-fail would reintroduce exactly the did-not-check-reads-as-checked ambiguity this lane removes, so it must be hard-fail or configurable-and-loud. **Make the absence explicit**: §6.4 asks for a `checks_performed` record on `VerifiedSigner` so the path builder states what it did *not* check rather than implying completeness. |
| **Name constraints** | **Not a silent skip.** `nameConstraints` (2.5.29.30) is absent from the recognised-critical set at `jar_signer.rs:2901`, and any unrecognised critical extension is refused at `:2911`. RFC 5280 §4.2.1.10 requires the extension to be critical, so a conforming constrained CA cannot pass unprocessed. | **Residue is the non-critical case only** — a non-conforming, non-critical `nameConstraints` is ignored. Filed as §6.3: recognise the OID and refuse regardless of criticality until processing exists. Also note `subjectAltName` *is* in the recognised set and is not processed; benign for code signing, where no name is used for authorisation, but it is a recognised-but-unprocessed critical extension and should be labelled as such. |
| **Multi-`SignerInfo`** | First only, silently. | **Cannot implement here.** Filed as §6.2 with a fail-closed interim: refuse a block carrying more than one `SignerInfo` rather than reporting the first as though it were the whole story. Refusing is safe — multi-signer JARs are rare, and today they are *mis*-reported. |
| Parent selection by DN only (no AKI) | Verified: `AuthorityKeyIdentifier` is parsed but not used to disambiguate. Both candidates must still pass the link-signature check, so this cannot forge a path — only fail to find a valid one. | No action. Fail-closed. |
| Clock | `SystemTime::now()`; an attacker who controls the clock controls expiry. | No action here. Needs a trusted time source or RFC 3161 timestamps. |

---

## 5. `unsafe` census

**Zero `unsafe` blocks, functions, traits, or impls in either crate.**

```
$ grep -rn "unsafe" native-builtins-crypto/src native-builtins-security/src
(no matches)
```

Per-file count, all zero: `bc_aes.rs`, `bc_chacha.rs`, `bc_newhope.rs`,
`bc_newhope_tables.rs`, `failure.rs`, `lib.rs`, `signature.rs` (crypto);
`lib.rs`, `sunec_intpoly.rs`, `sunec_point.rs` (security).

There is consequently nothing to document inline, no soundness invariant to
check, and no unchecked input reaching an `unsafe` block. Both crates are pure
safe Rust over fixed-size arrays and `Vec`, with all array indexing
bounds-checked by the compiler. The nearest thing to an unchecked invariant is
the **assumed** schedule/round-count precondition on the infallible kernels,
which is exactly what §1.1 and §1.2 turned from assumed into checked.

This row should be re-run rather than trusted if either crate gains a
dependency on a `-sys` crate or an intrinsic-based backend.

---

## 6. Changes needed outside these two crates

Ordered by exposure. Each is a specific edit; none could be made from this
lane.

### 6.1 `native-builtins/src/phases_late/bouncycastle.rs:6994` — a default round count

```rust
let rounds = match ctx.get_field_by_name(this, "rounds") {
    Value::Int(v) => v,
    _ => 20,
};
```

A round count that could not be read becomes `20`. Two problems: for a
`Salsa20/12` or `Salsa20/8` engine that silently encrypts under the wrong
round count, and — the real one — a by-name read of an unwritten `int` slot
returns `Value::Int(0)`, which the `Value::Int(v)` arm **accepts**. The
sibling reads in the same function (`engineState`, `x`, `keyStream`, `:7000`
onward) all refuse on a bad read; this one and `index` (`:7024`) do not.

Requested: make the `_` arm an `IllegalStateException` like its siblings, and
reject `v <= 0 || v % 2 != 0` explicitly at the read. Then migrate
`bc_stream_generate_key_stream` (`:6890`, `:6894`) to `try_chacha_core` /
`try_salsa_core` so the refusal is a catchable Java exception rather than the
abort this pass installed as a backstop.

### 6.2 `classloading/src/jar_signer.rs:478` — multi-`SignerInfo`

```rust
let first_si = signer_infos.into_iter().next()
    .ok_or("SignedData has no SignerInfo")?;
```

The rest are dropped with no count, no warning, and no marker on
`VerifiedSigner`. A caller reading `getCodeSource().getCertificates()` is told
"signed by X" with no way to learn that Y and Z also signed and were never
examined.

Requested (fail-closed interim, small): capture `signer_infos.len()` before
`next()`, and if it is greater than 1, refuse with a distinct message —
*"SignedData carries N SignerInfos; only single-signer blocks are verifiable —
refusing"*. Multi-signer JARs are rare and are currently *mis*-reported, so
refusing is a strict improvement. Full dispatch (and a decision about what
"signed by" means when signers disagree) is the larger follow-up.

### 6.3 `classloading/src/jar_signer.rs:2901` — non-critical `nameConstraints`

The recognised-critical set is `basicConstraints`, `keyUsage`,
`extKeyUsage`, `subjectKeyIdentifier`, `authorityKeyIdentifier`,
`subjectAltName`, `authorityInfoAccess`. `nameConstraints` (2.5.29.30) is
correctly absent, so a *critical* one is refused. A **non-critical** one is
silently ignored.

Requested: add an explicit `nameConstraints` check that refuses the chain
regardless of the critical bit, with its own `TrustError` reason, until §6.5
processing exists. Non-critical `nameConstraints` is non-conforming per RFC
5280 §4.2.1.10, so no conforming CA is affected.

### 6.4 `classloading/src/jar_signer.rs:228` — say what was *not* checked

`VerifiedSigner` carries `chain`, `principal`, `digest_alg`. Its doc comment
correctly says revocation, name constraints and policies are not established —
but that is a comment, and the type is what callers program against.

Requested: a `checks_performed` (or `checks_omitted`) field the path builder
fills in, so "we did not check revocation" is a value a caller can branch on
rather than a sentence in a doc comment. This is the mechanism that makes the
revocation gap *explicit* rather than *silent*, which is the only thing
available while revocation itself is unimplementable in the class loader.
Related: `find_class_code_source_info` `:2747` returns an empty cert vector for
four distinguishable conditions (unsigned, verification failed, directory
entry, no anchors configured) — that overloading should be resolved at the same
time.

### 6.5 Revocation and full RFC 5280 §6.1

The build order from `signed-jar-trust.md` §4 stands: CRL distribution-point /
OCSP fetch with a bounded cache and an explicit hard-fail policy; then the
name-constraint / `certificatePolicies` / policy-mapping state machine. Until
both exist, §0 of that document holds: **signed JARs are not a trust boundary
in this VM.**

### 6.6 Migrate the infallible kernel call sites to their `try_*` forms

* AES, six sites, all guarding with a `kw.len() >= 2` lower bound that still
  admits a 2- or 4-entry schedule: `bouncycastle.rs:6098`, `:6110`, `:6208`,
  `:6210`, `:6585`, `:6592`, plus the `use_aes` gates at `:7578`, `:8886`,
  `:9091`. `try_encrypt_block` / `try_decrypt_block` check the length exactly.
* ChaCha/Salsa, three sites that already re-check parity by hand and would be a
  pure consolidation: `:7237`, `:7264`, `:7287` — plus `:6890`/`:6894` from
  §6.1, which check nothing.

Each migration turns the abort this pass installed into a catchable Java
exception.

### 6.7 Delete `verify_rsa_pkcs1_v15`

Zero callers (§2). Deprecated in this pass; delete once the branch settles.

---

## 7. Test coverage added by this pass

All `#[cfg(test)]` inside the two crates.

| Property | Test |
|---|---|
| **Zero rounds is refused, and refused without writing a keystream** | `bc_chacha::tests::zero_rounds_is_refused_because_it_publishes_the_engine_state` |
| Negative even counts are refused like zero (not let through by a parity test) | `bc_chacha::tests::negative_even_round_counts_are_refused_like_zero` |
| The infallible ChaCha/Salsa/permute kernels abort rather than emit | `bc_chacha::tests::infallible_{chacha_core,salsa_core,permute}_aborts_on_zero_rounds`, `infallible_chacha_core_aborts_on_odd_rounds` |
| Odd counts still raise with BC's own message | `bc_chacha::tests::odd_round_counts_raise_illegal_argument` (unchanged) |
| The guard did not perturb the transform | `bc_chacha::tests::even_round_counts_succeed_and_match_the_infallible_kernels`, `try_chacha_core_matches_rfc8439_block` (unchanged; `0` removed from the accepted list) |
| A 2-entry AES schedule — which passes every caller's `>= 2` bound — aborts by name | `bc_aes::tests::infallible_encrypt_block_aborts_on_a_two_entry_schedule` |
| An empty schedule and a short block abort by name | `bc_aes::tests::infallible_decrypt_block_aborts_on_an_empty_schedule`, `infallible_encrypt_block_aborts_on_a_short_block` |
| **Group-order constants are exactly `n`**, proved from the backend's own canonicity test | `sunec_point::tests::group_order_constants_are_exact` |
| `n·G` is still the identity on all three curves (the guard did not false-positive on the one value that must survive) | `sunec_point::tests::identity_results_are_signalled_with_empty_coordinates`, `the_group_order_is_the_identity_on_every_supported_curve` |
| **A non-canonical scalar that is not `n` is refused, not answered with a plausible point** | `sunec_point::tests::non_canonical_scalars_other_than_the_group_order_are_refused` |
| The legacy `bool` verifier is still fail-closed | `signature::tests::bool_wrapper_is_fail_closed_on_every_error_path` (unchanged) |

No test in this pass asserts a certificate is valid at a hard-coded "now" —
neither crate parses certificates or evaluates validity windows at all, so the
expiring-fixture hazard does not arise here. No test uses a wall-clock bound.

---

## 8. Deliberately left out

* **Anything in `classloading/` or `native-builtins/`.** Not editable from this
  lane. Everything found there is in §6 with a specific edit.
* **Revocation, name-constraint processing, multi-`SignerInfo` dispatch.** Not
  implementable inside these two crates; the decision for each is in §4, and
  the fail-closed interim measures are in §6.2–§6.4.
* **The `bc_newhope::uniform` seed length.** Reasoning in §1.4 — `a` is a public
  parameter and BouncyCastle does not check either; a length check risks a false
  positive on the only caller.
* **`sunec_intpoly`'s `NullPointerException` / `ArrayIndexOutOfBoundsException`
  types.** Mis-typed against an ideal, but loud and catchable, never
  success-shaped. Changing them risks perturbing an unrelated caller for no
  failure-mode gain.
* **Deleting `verify_rsa_pkcs1_v15`.** Deprecated instead; §6.7.
* **A constant-time comparison for the group-order test in `scalar_mul_*`.** The
  operand reaching that comparison is already known non-canonical, i.e. not a
  real private key, and the order it is compared against is public.
