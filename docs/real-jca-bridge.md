# Real-JCA bridge — removing synthetic key intrinsics so real BouncyCastle keys flow

## Goal

Stop fabricating bare-interface `java/security/PublicKey`/`PrivateKey` synthetics
(which throw `ClassCastException ... cannot be cast to ECPrivateKey` in real
upstream code) and let the bundled BouncyCastle JCA provider run for real, so
`KeyPairGenerator.getInstance("ECDSA","BC").generateKeyPair()` yields concrete
`BCECPublicKey`/`BCECPrivateKey` that implement `java.security.interfaces.*`.

Repro suite: keycloak `crypto/default` (`BCECDSACryptoProviderTest`,
`DefaultCrypto*Test`, …) — see `reference_keycloak_test_harness` /
`reference_jca_synthetic_crypto_layers` memories.

## Two synthetic-key layers (both bypassed)

1. `native-builtins/src/crypto.rs` — `register_crypto_natives`, gated behind the
   `legacy-synthetic-crypto` cargo feature (a default). `crypto::crypto_impl`
   (real RustCrypto SHA/AES/RSA/ECDSA) is genuine and stays.
2. `native-builtins/src/jca/key_factory.rs` + `signature.rs` — registered
   unconditionally; the layer actually active in default real-JDK mode. Short-
   circuits `KeyPairGenerator/KeyFactory/Signature.getInstance` and returns
   bare-interface synthetics.

## `CRATONVM_REAL_JCA` gate (this branch)

`crate::real_jca_mode()` (lib.rs). When set:
- `jca::key_factory::register` / `jca::signature::register` early-return (no synthetic shims).
- `register_crypto_natives` and `register_key_generator_for_real_jdk` are skipped.
- The BouncyCastle EC provider `<clinit>`/`EC$Mappings.configure` no-ops in
  `provider_chain.rs` are skipped, so BC's *real* EC services register.

## The JCA bridge (this branch)

Real `KeyPairGenerator.getInstance(alg,"BC")` bytecode reaches
`sun.security.jca.GetInstance.getService(type,algorithm,provider)`, which does
`Providers.getProviderList().getProvider(provider)` — but our `Providers` shim
returns null → NPE before any `Provider` is consulted. Bridge (in
`jca/provider_chain.rs`, real-JCA mode only):

- `sun/security/jca/GetInstance.getService(String,String,String)` and the 2-arg
  search form → resolve a `Provider$Service` straight from our existing provider
  service map (the same map BouncyCastle populates via `Provider.put` /
  `parseLegacyPut`), bypassing the unmaterialised JDK-internal `ProviderList`.
- `java/security/Provider$Service.newInstance(Object)` → reflectively
  `new_object(className)` + `invoke("<init>","()V")` on the real BC `*Spi`, so
  the genuine BC keygen/sign bytecode runs. No synthetic key material is made
  here — these are pure JDK-bridge natives that route service resolution.

## Result (base commit 72640c2, JIT disabled)

The bridge **works**: real BouncyCastle EC bytecode executes — named-curve
parameter loading (`org.bouncycastle.asn1.x9.X9ECPoint.getPoint`,
`X9ECParametersHolder.getParameters`, `X9ECParameters`) runs to depth in ~30s
(no 5-minute interpreter walk). `getInstance("ECDSA","BC")` resolves a real BC
SPI via the bridge.

## Remaining blockers — all in the value-representation / GC-roots family

End-to-end green is blocked by bugs **outside this branch's scope**, being fixed
concurrently (see `docs/bc-ec-mod-mododdinverse-investigation.md`, the parallel
`kinds` type-tag work on the value stack + GC-roots fix in `value_stack.rs` /
`roots.rs`):

- **JIT on:** native SEGV (STATUS_ACCESS_VIOLATION) in a JIT frame during BC EC
  math — the documented allocate-then-putfield / long-bit-collision miscompile
  (BC is currently JIT-banned in `skip_list.rs` as a workaround).
- **JIT off:** repeated `implicit monitorexit on synchronized-method-frame-pop
  failed — thread does not own the monitor` on BC `synchronized` methods
  (`X9ECPoint.getPoint`, `X9ECParametersHolder.getParameters`). Root cause: the
  monitor object (`this`) was dropped from GC roots (the `CompactValue`
  long/object NaN-tag collision drops genuine object slots) — the exact case the
  concurrent `roots.rs` `kinds`-based root fix repairs.

## Integration plan

1. Land the concurrent value-stack `kinds` type-tag + GC-roots fix (not this
   branch — do not duplicate).
2. Rebase this bridge on top; re-run the keycloak `crypto/default` suite under
   `CRATONVM_REAL_JCA=1` (both JIT on and off) — expect real `BCEC*Key` to flow.
3. Once green across the suite, make real-JCA the default: delete the synthetic
   key shims (`jca/key_factory.rs` key/keypair paths, `crypto.rs` key shims),
   finish the `crypto_impl` top-level-module migration (so the feature-off build
   compiles), and retire the `CRATONVM_REAL_JCA` env gate.
