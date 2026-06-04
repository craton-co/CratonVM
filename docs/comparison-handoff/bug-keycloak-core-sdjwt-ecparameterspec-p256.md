# keycloak-core SdJwtTest — "Error obtaining ECParameterSpec for P-256 curve"

## TL;DR (status 2026-06-04)
- **Confirmed root cause:** the message is keycloak wrapping a `ClassCastException`.
  In default (synthetic-JCA) mode `KeyPairGenerator.getInstance("EC")` returns a
  stub (provider=null) whose `generateKeyPair()` yields a **bare
  `java.security.PublicKey` interface object** (not `ECPublicKey`), so keycloak's
  `(ECPublicKey) kp.getPublic()` throws. Providers register with **empty service
  tables** (`getService`→null, `getServices`→`IllegalStateException`).
- **The real SunEC path is correct** (`CRATONVM_REAL_JCA=1`): EC keygen/sign/verify
  all work and the full `SdJwtTest` passes `OK (2 tests)` (+`--nojit`; the
  gate-on-with-JIT `rc=1` is the *separate* JUnitCore-JIT issue).
- **No safe one-shot fix landed.** Two attempts were made and **reverted**:
  (1) flipping real-JCA default-on **breaks RSA + AES** (real path only seeds
  SunEC); (2) driving the real `ECKeyPairGenerator` SPI from the synthetic KPG
  fails because the SPI internally needs `AlgorithmParameters.getInstance("EC")` →
  the JCA provider machinery, whose bridges are **global** (can't scope to EC
  without breaking non-EC real-`getInstance` crypto like `MessageDigest`).
- **Recommended path:** shim `sun.security.util.ECUtil.getECParameterSpec`/
  `getECParameters` to resolve via `CurveDB` directly (no provider list), then
  delegate EC keygen **and** the EC `Signature` path to the real SunEC SPIs.
  Details in "UPDATE — option 2 … ATTEMPTED" and "Most promising remaining path"
  below.
- **Probes** (`keycloak-ecspec-probes/`): `ECSpecProbe`
  (AlgorithmParameters→spec), `ECProviders`/`SvcProbe` (empty service tables),
  `ECKeyGenProbe` (bare-key CCE), `ECDSAProbe` (full EC flow), `CryptoSmoke`
  (RSA/AES/EC/digest matrix — shows the flip breaking RSA/AES), `SpiProbe`
  (driving the real SPI directly).
- This is **distinct** from the bc-math-ec work (BouncyCastle's own interpreted EC);
  this is SunEC/JCA curve-parameter resolution.

## Symptom
Module `apps/keycloak/core` JUnit4 suite (run via `RunDirTests` over
`target/test-classes`):

| VM | tests | failures | wall |
|---|---|---|---|
| CratonVM-CPU | 101 | **2** | 42.5s |
| HotSpot (jdk-25) | 101 | 1 | 3.7s |

The **shared** failure on both VMs —
`JWKUtilTest.testBigInteger380bit48bytesErrorFor256` ("expected AssertionError …
nothing was thrown") — is a test-vs-JDK25 issue, NOT CratonVM.

The **CratonVM-specific** failure (passes on HotSpot):
```
FAIL settingsTest(org.keycloak.sdjwt.SdJwtTest)
     :: Error obtaining ECParameterSpec for P-256 curve
```

## Reproduce
```
CP="apps/_test-harness;apps/keycloak/core/target/classes;apps/keycloak/core/target/test-classes;$(cat apps/keycloak/core/cp.txt)"
target/release/java.exe --java-home "C:/Program Files/Java/jdk-25" -Xmx2g -cp "$CP" \
    org.junit.runner.JUnitCore org.keycloak.sdjwt.SdJwtTest
```
(HotSpot with the same CP passes.)

## Likely root cause
`SdJwtTest.settingsTest` builds an EC key/signature on **P-256** and the code
path fails to obtain an `java.security.spec.ECParameterSpec` for `secp256r1`.
On CratonVM this is the SunEC named-curve → `ECParameterSpec` lookup
(`sun.security.util.ECUtil.getECParameterSpec` / `AlgorithmParameters("EC")`
`.getParameterSpec(ECGenParameterSpec("secp256r1"))`). The "Error obtaining
ECParameterSpec" message is thrown by Keycloak/Nimbus when that returns null or
throws. Connects to the known JCA/SunEC provider-list gaps (see
`reference_jca_synthetic_crypto_layers`: the `sun.security.jca` provider-list
bridge + EC clinit). This is the **named-curve parameter lookup**, distinct from
the EC multiply-perf work — the parameter spec for P-256 isn't being produced.

## ROOT CAUSE (confirmed, 2026-06-04) — synthetic KeyPairGenerator returns bare keys
Investigated on branch `fix/keycloak-ecparameterspec` (worktree `C:/craton/CratonVM-ecspec`).

The "Error obtaining ECParameterSpec for P-256 curve" message is Keycloak
(`TestSettings.generateEcdsaKeySpec`) **wrapping the real exception**. The actual
failing call is `KeyUtils.generateEcKeyPair("...")`:
```java
KeyPairGenerator keyGen = KeyPairGenerator.getInstance("EC");
keyGen.initialize(new ECGenParameterSpec(name), new SecureRandom());
KeyPair kp = keyGen.generateKeyPair();
return ((java.security.interfaces.ECPublicKey) kp.getPublic()).getParams();  // <-- CCE here
```

Minimal repro (`bc-math-ec-probes/ECKeyGenProbe.java`), default (synthetic) mode:
```
KPG provider = null        (synthetic stub, not a real provider)
generateKeyPair() pub class = java.security.PublicKey     (a bare INTERFACE!)
-> ClassCastException: java/security/PublicKey cannot be cast to
   java/security/interfaces/ECPublicKey
```
The synthetic EC keygen (`native-builtins/src/jca/key_factory.rs`
`kpg_generate_key_pair` → `alloc_public_key`) computes a real EC keypair but wraps
it in `alloc_concurrent_synthetic(ctx, "java/security/PublicKey", 5)` — i.e. an
object whose class is the **interface** `PublicKey`, which does **not** implement
`ECPublicKey`. So `(ECPublicKey) pub` throws, Keycloak wraps it, test fails. This
is the documented bare-interface-key synthetic-layer problem
(`reference_jca_synthetic_crypto_layers`).

Provider state (default/synthetic): all 13 providers register but their service
tables are **empty** — `Provider.getService("KeyPairGenerator","EC")` returns null
and `Provider.getServices()` throws `IllegalStateException`. So `getInstance("EC")`
gets a no-provider synthetic stub.

## The real SunEC path works fully — the old "blocked" notes are STALE
With `CRATONVM_REAL_JCA=1` the synthetic JCA short-circuits are skipped and the
real JDK SunEC bytecode runs. Verified end-to-end:
- `ECSpecProbe` (AlgorithmParameters("EC").getParameterSpec) → OK, P-256.
- `ECKeyGenProbe` → provider `sun.security.ec.ECKeyPairGenerator`, pub
  `sun.security.ec.ECPublicKeyImpl`, `getParams()` non-null, 256-bit. OK.
- `ECDSAProbe` (keygen + `SHA256withECDSA` sign + verify) → `verify=true`. OK.
- **Full `SdJwtTest` with `CRATONVM_REAL_JCA=1 CRATONVM_DISABLE_JIT=1` → `OK (2
  tests)`, rc=0.**

So `reference_jca_synthetic_crypto_layers`' "real path blocked by provider-list
bridge + BC EC clinit no-op + interpreter long/float operand-stack bug" is no
longer true for the EC flow — `seed_sunec_services()` + the
`sun/security/jca/GetInstance.getService` bridge (provider_chain.rs, real-JCA
arm) now resolve real SunEC SPIs correctly.

Note: `CRATONVM_REAL_JCA=1` **with JIT on** gives a silent `rc=1` on the JUnitCore
harness — that is the *separate* `reference_jit_junitcore_corruption` issue
(JUnitCore.main JIT miscompile), NOT EC; it disappears with `--nojit`, and the
app-gauntlet `RunDirTests` harness doesn't go through `JUnitCore.main`.

## Fix options (the fix is "use the real SunEC path for EC")
The synthetic EC keygen can't cheaply produce a spec-compliant `ECPublicKey` (it
instantiates the bare `PublicKey` interface). The real SunEC path already does
everything correctly. Options:
1. **Flip `CRATONVM_REAL_JCA` default-ON** (with a `CRATONVM_SYNTHETIC_JCA=1`
   kill-switch). Smallest code change, aligns with the synthetic-stub-removal
   direction, and the real path is now functional. **Risk:** broad — affects RSA /
   AES / Cipher / all of JCA; needs a regression-pool + crypto-suite soak before
   shipping. (`real_jca_mode()` gates `cipher.rs`, `key_factory.rs`,
   `signature.rs`, `provider_chain.rs`, `lib.rs`.)
2. **Scope EC to real while leaving other algorithms synthetic.** Cleaner blast
   radius but harder to thread: the `register()` early-returns are all-or-nothing
   per method (algorithm is a runtime arg), so it needs the synthetic
   `KPG/KF/Signature/AlgorithmParameters.getInstance` natives to delegate to the
   real SunEC SPI when `alg ∈ {EC, ECDSA, *withECDSA}`, plus `seed_sunec_services()`
   enabled unconditionally.
3. Make the synthetic EC keygen return a concrete `ECPublicKey`/`ECPrivateKey`
   (a class implementing the interfaces + a `getParams()` native producing the
   P-256 `ECParameterSpec`). Keeps the synthetic layer but is against the
   stub-removal direction and is the most code.

Recommendation: option 1 behind a regression soak, or option 2 if a narrower
blast radius is required. Either way the underlying real path is proven.

### UPDATE — option 1 (global flip) TRIED and REJECTED (2026-06-04)
Flipping `real_jca_mode()` + `RealSelector.jca_group` default-on (kill-switch
`CRATONVM_SYNTHETIC_JCA=1`) was implemented and built. A crypto smoke test
(`keycloak-ecspec-probes/CryptoSmoke.java`, default mode) showed:
```
OK   MessageDigest SHA-256
FAIL AES-GCM round-trip -> NoSuchAlgorithmException: AES KeyGenerator not available
FAIL RSA keygen + sign/verify -> NoSuchAlgorithmException: RSA KeyPairGenerator not available
OK   EC keygen + ECDSA + getParams        <-- the keycloak fix works
OK   SecureRandom nextBytes
```
The real-JCA provider machinery only seeds **SunEC** (`seed_sunec_services()`);
there is no `seed_sunrsasign_services()` / `seed_sunjce_services()`, so with the
synthetic RSA/AES SPIs no longer registered, `KeyPairGenerator("RSA")` and
`KeyGenerator("AES")` resolve to nothing → "not available". The synthetic layer
exists precisely because the real path is **EC-only** today. The global flip was
**reverted**.

=> Option 1 is not viable without first seeding + validating the real SunRsaSign
(RSA KeyFactory/KeyPairGenerator/Signature) and SunJCE (AES KeyGenerator/Cipher/…,
~190 services) SPIs under CratonVM — a large, separate effort.

### UPDATE — option 2 (drive real SPI from synthetic KPG) ATTEMPTED; deeper than expected
Tried: make the synthetic `kpg_generate_key_pair` EC branch delegate to the real
`sun.security.ec.ECKeyPairGenerator` SPI (`new_object_initialized` + `invoke_virtual
initialize/generateKeyPair`, GC-pinned). **It doesn't work standalone in synthetic
mode:** `ECKeyPairGenerator.initialize(...)` internally calls
`sun.security.util.ECUtil.getECParameterSpec` → `getECParameters` →
**`AlgorithmParameters.getInstance("EC")`**, which NPEs in synthetic mode (no
provider). (An earlier `SpiProbe` "success" was a false positive — it ran on the
*flip* build with real-JCA on.) So the real SunEC keygen SPI itself depends on the
JCA provider machinery.

That machinery is **global, not EC-scopable**: the real arm of
`provider_chain.rs` wires `seed_sunec_services()` + `sun/security/jca/GetInstance.
{getService,getInstance,getServices}` + `Provider$Service.newInstance` bridges that
intercept **all** `getInstance` resolution. Enabling them unconditionally with only
EC seeded would break any crypto that uses *real* `getInstance` bytecode and isn't
seeded (e.g. `MessageDigest.SHA-256` — currently OK in synthetic mode). So you can't
cheaply turn the bridges on "for EC only". Reverted; worktree clean.

### Most promising remaining path: an EC-specific `ECUtil`/`CurveDB` shim
Avoid the provider machinery entirely: shim
`sun/security/util/ECUtil.getECParameterSpec(Provider, String)` (and/or
`getECParameters`) to resolve the named curve via `sun.security.util.CurveDB`
directly (a static curve registry, no provider list) instead of
`AlgorithmParameters.getInstance("EC")`. That unblocks the real
`ECKeyPairGenerator.initialize` standalone, so option 2's keygen delegation works
**without** touching the global bridges or RSA/AES. Still need to verify `CurveDB`
works standalone and to cover the EC **Signature** path the same way (the synthetic
`Signature` reads a synthetic `key_id` the real `ECPrivateKeyImpl` won't have, so
ECDSA sign/verify must also delegate to the real `sun.security.ec.ECDSASignature`
SPI). This is a focused but multi-method change — not a one-liner.

Net: the keycloak EC fix is a real sub-project (the synthetic JCA layer is
EC-incomplete and the real path's provider machinery is global). Root cause and the
exact remaining work are mapped above; no safe one-shot fix landed this session.

## What an agent should try next
1. On CratonVM, minimal probe:
   `AlgorithmParameters ap = AlgorithmParameters.getInstance("EC"); ap.init(new ECGenParameterSpec("secp256r1")); ap.getParameterSpec(ECParameterSpec.class)`
   — see whether it returns null / throws, and where.
2. Check the SunEC provider registration & `sun.security.ec.CurveDB` lookup for
   `secp256r1`/`1.2.840.10045.3.1.7` under CratonVM (real-bytecode vs native).
3. Likely the same provider-list bridge gap noted in
   `reference_classloader_gc_root_gap` / `reference_jca_synthetic_crypto_layers`.
   Fix so the named curve resolves; SdJwtTest.settingsTest should then pass.

## Scope
1 genuine CratonVM-specific failure in keycloak-core; the other 100 tests pass
identically to HotSpot. Not the EC-multiply perf issue — this is curve-parameter
resolution.
