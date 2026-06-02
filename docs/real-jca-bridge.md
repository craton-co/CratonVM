# Real-JCA bridge — removing synthetic key intrinsics so real BouncyCastle keys flow

## STATUS: real BC keys flow (commits a9244b5, 7882ae6 on dev)

`BCECDSACryptoProviderTest` (3/3), `DefaultCryptoKeyPairVerifierTest` (4/4 clean
`OK`), `DefaultSecureRandomTest` pass under `CRATONVM_REAL_JCA` (JIT off). Driver
(`/tmp/ecdrv/EcDriver`) proves `KeyPairGenerator("ECDSA","BC")` →
`BCECPrivateKey`/`BCECPublicKey`, `isECPrivateKey==true`, `getPublicFromPrivate`
round-trips equal. The original `ClassCastException` is gone.

### Remaining before flipping real-JCA to default (keep shims as opt-in, do NOT delete)
1. **JIT SEGV is a generic JUnit-harness-under-JIT miscompile, NOT crypto.** The
   driver runs the full keygen+derive JIT-ON and succeeds; every `JUnitCore` run
   SEGVs JIT-ON with zero progress output (crash during framework startup/JIT
   warmup, before any test). This is the broad allocate-then-putfield JIT-codegen
   bug (BC is already JIT-banned in `jit/skip_list.rs` for it). Real crypto apps
   work JIT-on; only the JUnit *test harness* under JIT crashes. → value/JIT-
   codegen domain (concurrent agent); a pragmatic correctness-first option is to
   JIT-skip the offending harness path.
2. **RSA / ECDH / PEM coverage gaps** (real per-algorithm work, not synthetic
   stubs): `DefaultCryptoRSAVerifierTest` → "Error creating X509v1Certificate" /
   `cannot construct SigAlgName` / RSA-cipher `block incorrect`; `BCEcdhEs...`,
   `PemUtilsBCTest` (3/6), `DefaultKeyStoreTypesTest` (2/3). These are genuine BC
   code paths the VM doesn't fully support yet (X509 cert generation, ECDH,
   PEM parse, keystore types).
3. **`Object→Locale` CCE** in `org.junit...SynchronizedRunListener.testRunFinished`
   (result-summary printing; intermittent) — generic value-corruption, makes
   `exit=1` even when tests pass.

Flipping real-JCA to default before (1)+(2) would regress the gauntlet (crypto-
using apps with unsupported algorithms) and crash JUnit-under-JIT. So: address
(1)+(2), then flip (real `real_jca_mode()` default true with synthetic kept as an
opt-out env gate; no code deleted — synthetic path retained for optimization).

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

## Update — integrated the agent's value-stack/GC/JIT fix (merge 9a8af62)

The concurrent agent's fix landed on branch `kc-springboot-wildfly-tests`
(`419a6f5 "Fix 7 VM bugs surfaced by the keycloak core+crypto-default test
suite"`), **not on `dev`**. Merged it into this branch (clean, no conflicts —
my files are `native-builtins/jca/*`+`lib.rs`, theirs are `vm/` core). Re-tested
`BCECDSACryptoProviderTest` under `CRATONVM_REAL_JCA=1`:

- **JIT on:** still SEGVs (same JIT frame) — the allocate-then-putfield / JIT
  codegen miscompile is NOT fully fixed for the BC EC path.
- **JIT off:** no longer crashes (exit 1, not 139) — progress — but the test
  errors `IllegalStateException: Not able to load any cryptoProvider`.

Root-caused the JIT-off failure with a minimal driver: in real-JCA mode,
`Class.forName("...DefaultCryptoProvider").getDeclaredConstructor().newInstance()`
returns a bare **`java.lang.Object`** (synthetic mode returns the correct class).
`DefaultCryptoProvider`'s ctor runs `new BouncyCastleProvider()` (heavy real BC
code) only in real-JCA mode. So `ServiceLoader` instantiates the provider, gets a
wrong-typed object, the `sorted(...).collect()` / cast yields CCE
(`Constable cannot be cast to CryptoProvider`) or an empty provider list → the
`IllegalState`. `native_constructor_new_instance` (lang_class.rs) allocates the
correct class and runs `<init>`; the returned `ObjectRef` is corrupted on the
**invoke/return path** — the same `CompactValue` long/object NaN-tag collision
family (cf. commit `e08017d "kinds-aware long decode on field/invoke/return
paths"`), surfacing here because the heavy BC ctor floods the long-collision band.

**Conclusion:** the agent's value-stack/GC fix is necessary but **not complete**.

### Additional moving-GC native bugs fixed on this branch (commit 0847afc)

Root-caused with a `--Xmx 6g` proxy test (large heap = no GC = correct result):
native code holding a raw `ObjectRef` across a re-entrant Java call that can
allocate returns a **stale** pointer (resolves to a reused `java.lang.Object`)
under the moving collector. Fixed by pinning in `thread.native_pin_roots` (which
GC forwards in place):
- `NativeContext::new_object_initialized` (alloc + `<init>` pinned) — used by
  `Constructor.newInstance`, `Class.newInstance`, `Provider$Service.newInstance`.
- `NativeContext::{pin_native_root,read_native_pin,unpin_native_roots}` — used by
  `service_loader.rs` to keep the providers `ArrayList` live across the
  forName/newInstance/add loop + `iterator()`.

Verified: reflective `newInstance` of a heavy-ctor class now returns the correct
type at default heap (was `java.lang.Object`); keycloak `CryptoIntegration` gets
**past** "Not able to load any cryptoProvider" into real `KeyPairGenerator(
"ECDSA","BC")` resolution via the bridge.

### Remaining: non-deterministic core value-corruption (concurrent agent's domain)

After the above, the EC test still fails — but the failure point **varies run to
run** (`Provider$Service.newInstance with no className`; `NoSuchMethodError
java/lang/Class.loadClass`; `ClassCastException Object cannot be cast to Locale`
in JUnit's result listener; JIT-on SEGV). This non-determinism is the signature
of the residual operand-stack `CompactValue` long/object NaN-tag corruption +
synchronized-method monitor-object loss — the same core value-representation /
JIT-codegen family the concurrent agent owns and has **not** fully closed. Do not
duplicate (branch-integration rule). The JCA bridge + GC-safety fixes are correct
and validated as far as the VM allows; end-to-end green needs that core fix
completed.

## Integration plan

1. Land the **complete** value-stack `kinds` type-tag + GC-roots + invoke/return
   + JIT-codegen fix (concurrent agent's domain — do not duplicate). Residual
   symptoms to drive it to closure: reflective `newInstance` → `java.lang.Object`
   under heavy-long ctors; monitor-ownership WARNs; BC-EC JIT SEGV.
2. Rebase this bridge on top; re-run the keycloak `crypto/default` suite under
   `CRATONVM_REAL_JCA=1` (both JIT on and off) — expect real `BCEC*Key` to flow.
3. Once green across the suite, make real-JCA the default: delete the synthetic
   key shims (`jca/key_factory.rs` key/keypair paths, `crypto.rs` key shims),
   finish the `crypto_impl` top-level-module migration (so the feature-off build
   compiles), and retire the `CRATONVM_REAL_JCA` env gate.
