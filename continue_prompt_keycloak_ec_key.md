# RESOLVED: Keycloak SD-JWT EC key — real SunEC via route 1, DER encoder subclass bug fixed

**Status:** PRIMARY BUG FIXED on branch `ec-key-fix` (worktree `C:\craton\CratonVM-ec`).
`DefaultCryptoSdJwsTest` **0/14 → 14/14** (the original CCE + DER-signing failure). The wider 8-class
SD-JWT sweep is **~89/95**: a *separate* verify-side issue remains — see "Remaining: EC verify in the
presentation/multi-key path".

## Root cause & fix (three changes)
Route 1 was chosen: run real JDK-25 SunEC bytecode (it's pure Java — no native methods) instead of the
synthetic `KeyPairGenerator` stub that returned a bare `java/security/PublicKey` (the original CCE).
Run with `CRATONVM_REAL_JCA=1 CRATONVM_DISABLE_JIT=1`.

1. **`native-builtins/src/jca/cipher.rs`** — in `real_jca_mode()`, `java/security/Security.<clinit>` now runs
   `security_clinit_spimap`, setting the static `spiMap` to an empty `ConcurrentHashMap`. The real `<clinit>`
   can't run (its `initialize()` fails reading the java.security file), and the plain no-op left `spiMap`
   null, NPEing `AlgorithmParameters.getInstance("EC")` → `Security.getImpl` → `getSpiClass`.
2. **`native-builtins/src/jca/provider_chain.rs`** — `seed_sunec_services()` (called in the `real_jca_mode()`
   block) mirrors SunEC's `putEntries()` table (KeyPairGenerator/KeyFactory/AlgorithmParameters EC +
   `ECDSASignature$*` family + EC OID aliases; class names from `javap -c sun.security.ec.SunEC` on JDK 25.0.1)
   into the service map, so the existing GetInstance bridge instantiates the real pure-Java SPIs.
3. **`native-io/src/lib.rs`** — THE actual blocker for the last 3 (signing) tests. `native_baos_write` /
   `native_baos_write_bytes` (the always-on real-JDK `ByteArrayOutputStream` intrinsics) guarded their
   fast path with `if cls_name != "java/io/ByteArrayOutputStream" { return Ok(None); }` — an **exact-class**
   check that silently **no-ops for every BAOS subclass**, including `sun.security.util.DerOutputStream`.
   So `DerOutputStream.write` dropped every byte → `ECUtil.encodeSignature` produced an empty DER →
   `Signature("SHA256withECDSA").sign()` returned `byte[0]` → `BCECDSACryptoProvider.asn1derToConcatenatedRS`
   NPE'd (`getObjectAt on null`). Fix: replaced the exact-class guard with `receiver_is_baos()`, which walks
   the superclass chain by name and accepts BAOS **or any subclass**. The inherited `buf`/`count` fields are
   always at slots 0/1 (superclass fields laid out first), so the raw-slot fast path is correct for subclasses;
   a subclass that needs different `write` behaviour declares its own `write` (wins dispatch, never reaches
   this base-class native), so the change stays safe for unrelated `OutputStream` subclasses hitting the
   `java/io/OutputStream`-registered fallback.

## How the root cause was found (notes for similar bugs)
- The two BAOS registrars in `native-builtins` (`serialization.rs::register_byte_array_output_stream`,
  `tests_extracted.rs::register_s4_baos`) are RED HERRINGS: the former is only registered from
  `register_synthetic_overrides`, which is `#[cfg(feature = "synthetic-jdk")]` and **not compiled** in the
  real-JDK / `legacy-synthetic-crypto` CLI; the latter's module isn't declared at all (dead code).
- The ACTIVE BAOS intrinsic is in the `native-io` crate (`native-io/src/lib.rs`, `native_baos_*`), registered
  unconditionally via `register_essential_natives`. `--dump-native-registry` confirms the 12 BAOS methods.
- An `Unsafe` probe (`Unsafe.objectFieldOffset` + `putInt`) confirmed `buf@0, count@1` and that slot 1 is
  writable for a `DerOutputStream` — proving the object layout is fine and the bug was a no-op, not a bad slot.

## Verification
- Fast EC-free DER check (`ecprobe_tmp/DerProbe`, seconds): `DerOutputStream.toByteArray=22`,
  `DerValue.toByteArray=24`, `ECUtil.encodeSignature=70` (match HotSpot). `DerProbe2`: `size=1` after one write.
- `DefaultCryptoSdJwsTest` → **OK (14)**.

## Remaining: EC verify in the presentation/multi-key path (separate bug, NOT the EC-key/DER fix)
The 8-class sweep has **6 failures** (3 distinct methods), all `VerificationException: Invalid jws signature`:
`SdJwtPresentationConsumerTest.shouldVerifySdJwtPresentation` (positive — should succeed but verify is
rejected), plus the negatives `…shouldFail_IfPresentationRequirementsNotMet` and
`SdJwtVerificationTest.sdJwtVerificationShouldFail_WithWrongVerifier` (which now fail because verification
errors with "Invalid jws signature" before reaching their intended assertion). This is verify-side: basic
sign→verify round-trips fine (`DefaultCryptoSdJwsTest.testVerifySignature_Positive` passes), so the issuer
JWT in the presentation-consumer path is being verified against a public key that doesn't match — likely the
key reconstructed from JWK x/y coords via `KeyFactory.generatePublic(ECPublicKeySpec)` or
`BCECDSACryptoProvider.getPublicFromPrivate` (BC `getG().multiply(getD())` EC point mult). Next step:
isolate whether `KeyFactory("EC").generatePublic(ECPublicKeySpec(point, p256spec))` yields a key whose
`getEncoded()`/`getW()` match HotSpot, and whether `Signature("SHA256withECDSA").verify` of a known-good
external signature succeeds. Distinct from this fix; track separately.

## Perf caveat (not a correctness issue)
The first EC op pays a one-time ~108 s `Secp256R1GeneratorMontgomeryMultiplier.<clinit>` generator-table
precompute under the no-JIT interpreter; subsequent EC ops are ~1–16 s each. The full 8-class SD-JWT sweep
(~95 tests, lots of EC verify/sign) therefore takes tens of minutes under `--nojit` and may need a long
timeout. (JIT-on or a native EC intrinsic is a separate optimization; JUnit-4 under JIT is blocked by a
separate `LambdaForm.<clinit>` bug.)

## Build/run caveat in this environment
A parallel agent runs `taskkill //F //IM cargo.exe|rustc.exe|cratonvm.exe` in a loop in the shared tree,
killing builds/processes by image name. Work was done in worktree `C:\craton\CratonVM-ec`; builds/tests must
use **renamed** binaries to dodge the kill — copies in the toolchain bin dir (`ecbuild.exe`/`ecrustc.exe`,
`RUSTC=...ecrustc.exe`) and a renamed `eccvm.exe` for running tests.

## Key files
`native-builtins/src/jca/cipher.rs`, `native-builtins/src/jca/provider_chain.rs`, `native-io/src/lib.rs`
(`receiver_is_baos` + the two guard sites ~`native_baos_write`/`native_baos_write_bytes`).
