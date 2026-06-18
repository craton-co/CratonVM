# Fix note — nb-security-pqc (S1: PQC keygen fail-closed, no synthetic stubs)

## Finding

Report `nb-security.md` S1: the default JCA path generated **synthetic/empty key
material** for the post-quantum algorithms (`ML-KEM-512/768/1024`, `ML-DSA-44/65/87`,
a.k.a. Kyber/Dilithium) and the unimplemented classical extras (`Ed25519`, `X25519`).
`algo_idx` recognised those names, but `kpg_generate_key_pair` only implemented RSA and
EC. Everything else fell through to a fallback that allocated `PublicKey`/`PrivateKey`
synthetics with **empty DER and `key_id == 0`** — a `KeyPair` presented as a successful
keygen even though it carried no usable key material. Any app that then signed /
encapsulated / verified with such a key got garbage or a silent false-success. That is
exactly the "synthetic stub faking app behavior" the project forbids
(`feedback_no_synthetic_stubs.md`).

## Root cause

`kpg_generate_key_pair` (and the `KeyFactory` import paths `kf_generate_public` /
`kf_generate_private`) had no fail-closed terminal arm: any recognised-but-unimplemented
algorithm, or an RSA/EC `KeySpec` that failed to parse, dropped into a synthetic
`alloc_public_key`/`alloc_private_key` with empty DER + `key_id == 0` instead of raising
the JDK-contractual checked exception.

## Exact change

The fix is **already present** in the current `dev` working tree
(`native-builtins/src/jca/key_factory.rs`, shown as `M` in git status — landed by an
earlier round of this same task). I verified it is complete and correct; **no further
source edit was required**. Concretely, the file now:

1. Adds throw helpers built on the existing exception pattern (`new_object_initialized`
   of a real JCA exception class → `MethodCallFailed::ExceptionThrown`, with a
   `RuntimeError::SecurityException` defensive fallback if the class can't be
   constructed): `throw_jca`, `throw_no_such_algorithm`
   (`java/security/NoSuchAlgorithmException`), `throw_invalid_key_spec`
   (`java/security/spec/InvalidKeySpecException`). (key_factory.rs:407–444)
2. `kpg_generate_key_pair`: after the RSA (idx 6) and EC (idx 7) arms return real keys,
   the terminal arm now throws `NoSuchAlgorithmException`
   (`"<algo> KeyPairGenerator not available"`) for ML-KEM / ML-DSA / Ed25519 / X25519 /
   unknown — no empty-key fallback. (key_factory.rs:590–599)
3. `kf_generate_public`: throws `InvalidKeySpecException` when no usable key can be
   produced (unimplemented algorithm, or an RSA/EC spec that fails to parse) rather than
   returning a `key_id == 0` key. (key_factory.rs:717–728)
4. `kf_generate_private`: only the real SunEC EC path can import a private key; every
   other algorithm now throws `InvalidKeySpecException`. (key_factory.rs:755–761)

I confirmed the types/APIs these arms use all exist and match: `RuntimeError::SecurityException { message: String }` (types/src/error.rs:299), `MethodCallFailed::ExceptionThrown(ObjectRef)` (types/src/error.rs:43), `crate::route_ec_to_real()`/`crate::real_jca_mode()` (native-builtins/src/lib.rs:538,562), and `MockNativeContext` (test_utils.rs) provides `new_object`/`create_string`/`get_field`/`set_field`/`array_length`; `new_object_initialized` resolves to the default trait impl (native-api/src/registry.rs:305) so the test's exception construction yields `ExceptionThrown`.

## No RSA/EC/AES regression

- **RSA** (idx 6) keygen still returns real keypairs (`crypto_impl::Rsa::generate_keypair`
  + real DER + non-zero `key_id`); RSA public import still parses
  `SubjectPublicKeyInfo` via `crypto_impl::parse_rsa_public_key`. Untouched.
- **EC** (idx 7) still routes to the real SunEC SPI under `route_ec_to_real()` (default
  on), or the `crypto_impl::Ecdsa` software path under the `CRATONVM_SYNTHETIC_EC=1`
  kill-switch. Untouched.
- **AES** is not handled in this file (it lives in `jca/cipher.rs`); no change here can
  affect it.
- The fail-closed arms are strictly *after* the RSA/EC success returns, so they cannot
  intercept the working paths. Guarded by tests
  `rsa_keygen_still_returns_real_key_material` and
  `keyfactory_rsa_public_import_still_returns_real_key`.

## Files touched

- `native-builtins/src/jca/key_factory.rs` — fix verified present (no new edit needed
  this round; landed by an earlier round of this task).
- **Not touched / does not exist:** `native-builtins/src/jca/key_pair_generator.rs` was
  listed as an owned file, but there is no such file. The `KeyPairGenerator` natives live
  inside `key_factory.rs` (the `kpg_*` functions). Nothing to change there.

## Note: other synthetic PQC layer NOT in my owned files

The report (and module doc in `key_factory.rs`) reference a parallel synthetic KPG in
`native-builtins/src/crypto.rs` (`KPG_ALGORITHMS`, `register_key_pair_generator`). That
file is **feature-gated `legacy-synthetic-crypto`** and only wired into
`register_synthetic_overrides`, so it does **not** run in the default real-JDK build —
the default path is the `key_factory.rs` one fixed here. If `crypto.rs` still mints
empty PQC keys under that feature, it should get the same fail-closed treatment, but it
is outside my owned files (`crypto.rs` is owned by another agent / the nb-security
crypto-impl scope). Flagging precisely so it isn't lost.

## Tests added

No new tests added this round — the relevant tests already exist in
`key_factory.rs` and I confirmed they compile against the available APIs:

- `unimplemented_algorithm_keygen_throws_not_empty_key` — ML-KEM/ML-DSA/X25519/Ed25519/
  bogus `generateKeyPair` must throw `ExceptionThrown`, never return an empty key.
- `keyfactory_unproducible_key_throws_not_dead_key` — unimplemented-algorithm and
  unparseable-RSA-DER `generatePublic`/`generatePrivate` must throw.
- `rsa_keygen_still_returns_real_key_material` — RSA keygen still yields real DER +
  non-zero `key_id` (regression guard).
- `keyfactory_rsa_public_import_still_returns_real_key` — valid RSA SPKI still imports.
- `algo_idx_round_trip` / `algo_name_round_trip`.

## Follow-up & risk

- **Risk: low / none.** No source change made this round; the existing fix is surgical
  and additive (terminal throw arms after the working returns). RSA/EC/AES paths are
  provably unaffected.
- **Follow-up 1:** apply the same fail-closed terminal arm to the legacy
  `crypto.rs::register_key_pair_generator` PQC entries (gated `legacy-synthetic-crypto`)
  so the synthetic-mode build is also fail-closed — needs the owner of `crypto.rs`.
- **Follow-up 2 (optional, larger):** back ML-KEM/ML-DSA with a vetted PQC crate to turn
  the `NoSuchAlgorithmException` into real support.
