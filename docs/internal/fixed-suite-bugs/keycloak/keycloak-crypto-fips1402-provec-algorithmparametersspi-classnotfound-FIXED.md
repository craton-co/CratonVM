# crypto/fips1402: BC-FIPS `EngineCreator`-registered services misresolved via bogus `className` guessing — FIXED

Status: fixed on 2026-07-07 (branch `fix/keycloak-fips1402-provec-classname-20260707`) by
reaching BouncyCastle-FIPS's private `creatorMap` directly instead of reflectively
guessing a class name from cosmetic registration metadata.

## Root cause

CratonVM's real-JCA bridge (`../../../../native-builtins/src/jca/provider_chain.rs`) assumed every
JCA `Provider` registers each algorithm with a `className` string that is directly
`Class.forName`-loadable, and always tried to instantiate the SPI that way
(`build_jca_instance` / `provider_service_new_instance`). That assumption holds for
ordinary providers (SunEC, SunJCE, plain BouncyCastle's `addAlgorithm(key, className)`),
but **not** for `BouncyCastleFipsProvider`.

BC-FIPS's own `addAlgorithmImplementation(String algorithm, String className,
EngineCreator creator)` (decompiled via `javap` from `bc-fips-2.1.2.jar`) does:

```
this.put(key, className);                 // captured into our ServiceEntry.class_name
this.creatorMap.put(className, creator);   // a PRIVATE Map<String,EngineCreator>, invisible to us
```

`className` here is built as `<enclosing-class>.getName() + "." + "AlgorithmParametersSpi$EC"`
(a hybrid string mixing a dotted FQCN with a `$`-nested member name purely as a *label*)
— e.g. `"org.bouncycastle.jcajce.provider.ProvEC.AlgorithmParametersSpi$EC"`. It was never
meant to be loaded: BC-FIPS's real `Provider$Service` subclass
(`BouncyCastleFipsProvider$BcService`) overrides `newInstance(Object)` to call
`creator.createInstance(param)` directly, bypassing reflection entirely — but that override
never actually ran, because CratonVM's native registry intercepts `Provider.getService`/
`Provider$Service.newInstance` on the *ancestor* `java/security/Provider`/`Provider$Service`
classes regardless of a subclass's real-bytecode override.

Naively `.replace('.', '/')`-ing the hybrid label string produced
`org/bouncycastle/jcajce/provider/ProvEC/AlgorithmParametersSpi$EC` — a path that matches
no real class anywhere in the jar (confirmed: no `ProvEC$AlgorithmParametersSpi` or
`AlgorithmParametersSpi$EC` class exists in `bc-fips-2.1.2.jar`; the real EC
`AlgorithmParameters` SPI is the differently-named `ProvEC$ECAlgParams`, instantiated
directly by `new` inside an anonymous `EngineCreator` — `ProvEC$3.createInstance()` for
this particular algorithm).

A secondary, independent bug compounded this: the resulting `ClassNotFoundException`
was raised via the low-level `new_object_initialized`/`load_class_concurrent` bootstrap
path, whose failures are **non-catchable internal `VmError`s** by design (meant for
genuine VM-internal class-loading, not arbitrary provider-supplied strings) — so instead
of a normal Java exception, the whole process aborted:
```
Error in thread "main" class file error: class not found: org/bouncycastle/jcajce/provider/ProvEC/AlgorithmParametersSpi$EC
```

## Fix

Two changes in `../../../../native-builtins/src/jca/provider_chain.rs`:

1. **`try_engine_creator_instantiate`** — given a real `Provider` object and a
   `className` string, reads the object's `creatorMap` field (if present), looks up
   `className` in it, and if a creator is on file, calls `EngineCreator.createInstance(Object)`
   on it directly — the exact call BC-FIPS's own (unreachable) `BcService.newInstance`
   would have made. Falls through to the existing reflective path when no `creatorMap`
   field exists (every ordinary provider).
2. **`real_provider_table`** — `Security.addProvider`/`insertProviderAt` now retain the
   *real* `Provider` object (via a permanent GC root — `NativeContext::add_global_root`),
   keyed by provider name, so `build_jca_instance` (used by the no-explicit-provider
   `AlgorithmParameters.getInstance("EC")` search path, which is what the failing tests
   actually hit via `sun.security.util.ECUtil.getECParameters()` → `Security.getImpl`)
   can reach `creatorMap` — previously every read handed out a **fresh synthetic**
   `Provider` object with no such field.
3. Defense in depth: `build_jca_instance`/`provider_service_new_instance` now explicitly
   convert a `new_object_initialized`/class-loading failure into a catchable
   `RuntimeError::ClassNotFoundException` instead of letting the internal `VmError`
   propagate and abort the process — so *any* similarly-bogus provider-supplied
   `className` (not just this one) fails as an ordinary Java exception, not a crash.

## Validation

Direct repro via `KcRunner` on the real `crypto/fips1402` classpath
(`../../../../apps/keycloak/crypto/fips1402`, `bc-fips-2.1.2.jar` from `~/.m2`), JIT on:

- `BCFIPSECDSACryptoProviderTest` — the documented crash
  (`class not found: .../ProvEC/AlgorithmParametersSpi$EC`, whole-process abort) is
  **gone**. `AlgorithmParameters.getInstance("EC")` now correctly returns a real
  `ProvEC$ECAlgParams` built via BC-FIPS's own `EngineCreator`. At this point,
  1/3 parameterized cases still failed on the separate P-384 keygen issue, later
  fixed in [keycloak-crypto-fips1402-sunec-keypairgenerator-384bit-gap-FIXED.md](keycloak-crypto-fips1402-sunec-keypairgenerator-384bit-gap-FIXED.md).
- `BCFIPSEcdhEsAlgorithmProviderTest` - same: crash gone; at this point, 1/2 tests
  still failed on the separate P-384 keygen issue fixed in the follow-up note.
- `FIPS1402SdJwtCreationAndSigningTest` - same: crash gone; at this point, 1/2 tests
  still failed on the separate P-384 keygen issue fixed in the follow-up note.
  `ProvEC$ECAlgParams` built via BC-FIPS's own `EngineCreator`. 1/3 parameterized cases
  now fail on an unrelated, pre-existing issue — see
  [crypto-fips1402-sunec-keypairgenerator-384bit-gap.md](../../known-issues/crypto-fips1402-sunec-keypairgenerator-384bit-gap.md).
- `BCFIPSEcdhEsAlgorithmProviderTest` — same: crash gone, 1/2 tests pass, the
  remaining failure is the same unrelated 384-bit gap.
- `FIPS1402SdJwtCreationAndSigningTest` — same: crash gone, 1/2 tests pass, remaining
  failure is the same unrelated 384-bit gap.
- `FIPS1402JwtVcMetadataTrustedSdJwtIssuerTest` (the OTHER known-issues doc,
  `crypto-fips1402-sdjwt-hang-after-provider-init.md`) — **16/16 PASS**, no hang. See
  that doc's own FIXED writeup for detail; it shares this exact root cause (the hang
  was this same `AlgorithmParameters`/`Signature` resolution path, for a curve/algorithm
  combination that didn't happen to hit the fatal-abort variant).
- `cargo test -p cratonvm-native-builtins --features synthetic-jdk jca::provider_chain`:
  22/22 existing unit tests still pass (no regression to ordinary, non-BC-FIPS provider
  resolution).

## Follow-up Fixed

The separate P-384/ES384 keygen residual described at the time of this fix is now fixed
and archived in [keycloak-crypto-fips1402-sunec-keypairgenerator-384bit-gap-FIXED.md](keycloak-crypto-fips1402-sunec-keypairgenerator-384bit-gap-FIXED.md). The follow-up routes explicit BC-FIPS EC/ECDSA keygen through the provider-created BC-FIPS generator and avoids SunEC's no-arg constructor default-size initialize for no-provider `ECGenParameterSpec` keygen.
`sun.security.ec.ECKeyPairGenerator.initialize` throws
`InvalidParameterException: No EC parameters available for key size 384 bits` for the
P-384/ES384 case across all three tests above. This is unrelated to the
`className`/`EngineCreator` bug fixed here — `KeyPairGenerator.getInstance("ECDSA",
"BCFIPS")` resolves to **SunEC's own** `ECKeyPairGenerator` (not a BC-FIPS SPI) via a
pre-existing, separate CratonVM EC-routing shortcut, and that path's curve-size table
doesn't cover 384 bits. Tracked in
[crypto-fips1402-sunec-keypairgenerator-384bit-gap.md](../../known-issues/crypto-fips1402-sunec-keypairgenerator-384bit-gap.md).

---

# Historical report: nested class `ProvEC$AlgorithmParametersSpi$EC` not found, crashing whole process

Historical original status: open — genuine classloading gap, crashes the entire VM
process (not a catchable Java exception).

Date observed: 2026-07-07 (local-host 4-shard rerun, branch
fix/keycloak-nonpassed-rerun-local-20260707)

## Summary

5 distinct `crypto/fips1402` test classes all CRASHed with the exact same signature:

```
[cratonvm] main-vm run() returned Err: Error in thread "main" class file error: class not found: org/bouncycastle/jcajce/provider/ProvEC/AlgorithmParametersSpi$EC
```

Affected classes: `BCFIPSECDSACryptoProviderTest`, `BCFIPSEcdhEsAlgorithmProviderTest`,
`sdjwt.FIPS1402SdJwtCreationAndSigningTest`, `sdjwt.FIPS1402SdJwtKeyBindingTest`,
`sdjwt.FIPS1402SdJwtVPTest` — every one of them failed identically, always right after
`FIPS1402Provider created: KC(BCFIPS version 2.0102, ...)` logs successfully (i.e. FIPS
provider bootstrap itself works fine; the failure is specifically when something tries
to instantiate BouncyCastle's EC `AlgorithmParameters` SPI implementation).

## Notes (original, at filing time)

- `org.bouncycastle.jcajce.provider.ProvEC$AlgorithmParametersSpi$EC` is a nested class
  (`AlgorithmParametersSpi` nested inside the `ProvEC` provider-registration class, with
  `EC` a further-nested implementation class) — this is standard BouncyCastle structure
  for registering per-algorithm `AlgorithmParameters` SPIs. "class not found" (not
  `NoClassDefFoundError` or `ClassNotFoundException` at the Java level, but a VM-level
  "class file error") suggests CratonVM's classloading can't locate/resolve this
  specific nested class within the BC jar — possibly a nested-class name-mangling
  issue (`$` handling), a JAR entry lookup gap, or an eager-resolution path that doesn't
  correctly walk into doubly-nested provider classes. **(Resolved finding: the string
  was never a real class at all — see Root cause above; the "nested class" framing was
  a reasonable but incorrect first guess.)**
- All 5 affected classes involve EC (Elliptic Curve) cryptography specifically —
  ECDSA, ECDH-ES, and SD-JWT (which likely uses EC keys for its examples) — consistent
  with this being an EC-`AlgorithmParameters`-specific gap rather than a general BC
  provider registration problem.
- This crashes the **whole process** rather than surfacing as a catchable Java
  exception — consistent with the pattern already noted in the sibling
  `crypto-elytron-missing-jca-engine-registrations.md` doc from this same investigation
  session, where some "algorithm/class not found" cases panic the VM instead of
  throwing a normal `NoSuchAlgorithmException`/`ClassNotFoundException`.

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-local-20260707
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-provec-algparams -ClassList <(printf 'module\tclass\ncrypto/fips1402\torg.keycloak.crypto.fips.test.BCFIPSECDSACryptoProviderTest\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-local-20260707.exe -JdkHome $jdk
```

## Evidence

`C:\craton\CratonVM-keycloak-nonpassed-local-20260707\apps\keycloak-suite-runner\.suite\results\nonpassed-local-20260707-shard1\others-jit\logs\crypto_fips1402.org.keycloak.crypto.fips.test.{BCFIPSECDSACryptoProviderTest,BCFIPSEcdhEsAlgorithmProviderTest,sdjwt.FIPS1402SdJwtCreationAndSigningTest,sdjwt.FIPS1402SdJwtKeyBindingTest,sdjwt.FIPS1402SdJwtVPTest}.err.log`, 2026-07-07 local 4-shard rerun with 1200s timeout.
