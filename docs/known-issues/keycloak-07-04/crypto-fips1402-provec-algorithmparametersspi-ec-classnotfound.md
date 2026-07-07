# crypto/fips1402: nested class `ProvEC$AlgorithmParametersSpi$EC` not found, crashing whole process

Status: open — genuine classloading gap, crashes the entire VM process (not a catchable Java exception)

Date observed: 2026-07-07 (local-host 4-shard rerun, branch fix/keycloak-nonpassed-rerun-local-20260707)

## Summary

5 distinct `crypto/fips1402` test classes all CRASH with the exact same signature:

```
[cratonvm] main-vm run() returned Err: Error in thread "main" class file error: class not found: org/bouncycastle/jcajce/provider/ProvEC/AlgorithmParametersSpi$EC
```

Affected classes: `BCFIPSECDSACryptoProviderTest`, `BCFIPSEcdhEsAlgorithmProviderTest`,
`sdjwt.FIPS1402SdJwtCreationAndSigningTest`, `sdjwt.FIPS1402SdJwtKeyBindingTest`,
`sdjwt.FIPS1402SdJwtVPTest` — every one of them fails identically, always right after
`FIPS1402Provider created: KC(BCFIPS version 2.0102, ...)` logs successfully (i.e. FIPS
provider bootstrap itself works fine; the failure is specifically when something tries
to instantiate BouncyCastle's EC `AlgorithmParameters` SPI implementation).

## Notes

- `org.bouncycastle.jcajce.provider.ProvEC$AlgorithmParametersSpi$EC` is a nested class
  (`AlgorithmParametersSpi` nested inside the `ProvEC` provider-registration class, with
  `EC` a further-nested implementation class) — this is standard BouncyCastle structure
  for registering per-algorithm `AlgorithmParameters` SPIs. "class not found" (not
  `NoClassDefFoundError` or `ClassNotFoundException` at the Java level, but a VM-level
  "class file error") suggests CratonVM's classloading can't locate/resolve this
  specific nested class within the BC jar — possibly a nested-class name-mangling
  issue (`$` handling), a JAR entry lookup gap, or an eager-resolution path that doesn't
  correctly walk into doubly-nested provider classes.
- All 5 affected classes involve EC (Elliptic Curve) cryptography specifically —
  ECDSA, ECDH-ES, and SD-JWT (which likely uses EC keys for its examples) — consistent
  with this being an EC-`AlgorithmParameters`-specific gap rather than a general BC
  provider registration problem (RSA/HMAC-based fips1402 tests pass or hit the
  unrelated FIPS-mode-skip `Assume` pattern instead, not this crash).
- This crashes the **whole process** rather than surfacing as a catchable Java
  exception — consistent with the pattern already noted in the sibling
  `crypto-elytron-missing-jca-engine-registrations.md` doc from this same investigation
  session, where some "algorithm/class not found" cases panic the VM instead of
  throwing a normal `NoSuchAlgorithmException`/`ClassNotFoundException`.

## Next steps

1. Find where CratonVM resolves nested-class lookups within JARs (classloading crate)
   and check specifically how it handles doubly-nested classes (`Outer$Middle$Inner`
   pattern) — compare against a minimal repro loading
   `org.bouncycastle.jcajce.provider.ProvEC$AlgorithmParametersSpi$EC` directly via
   `Class.forName()` outside of the FIPS provider bootstrap path, to isolate whether
   this is a general nested-class gap or specific to how BC's `Provider.put()`-based
   registration triggers the lookup.
2. Verify whether other similarly-nested BC provider classes (e.g. other
   `AlgorithmParametersSpi` implementations for RSA, DSA, etc.) load successfully,
   to narrow whether this is EC-specific or a broader doubly-nested-class gap that
   happens to only be exercised by EC-using tests in this particular class range.

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-local-20260707
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-provec-algparams -ClassList <(printf 'module\tclass\ncrypto/fips1402\torg.keycloak.crypto.fips.test.BCFIPSECDSACryptoProviderTest\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-local-20260707.exe -JdkHome $jdk
```

## Evidence

`C:\craton\CratonVM-keycloak-nonpassed-local-20260707\apps\keycloak-suite-runner\.suite\results\nonpassed-local-20260707-shard1\others-jit\logs\crypto_fips1402.org.keycloak.crypto.fips.test.{BCFIPSECDSACryptoProviderTest,BCFIPSEcdhEsAlgorithmProviderTest,sdjwt.FIPS1402SdJwtCreationAndSigningTest,sdjwt.FIPS1402SdJwtKeyBindingTest,sdjwt.FIPS1402SdJwtVPTest}.err.log`, 2026-07-07 local 4-shard rerun with 1200s timeout.
