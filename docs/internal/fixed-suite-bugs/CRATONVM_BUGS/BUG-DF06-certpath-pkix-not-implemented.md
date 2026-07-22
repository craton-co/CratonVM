# Bug DF06 — `CertPathValidator` PKIX not implemented → OCSP / cert-path tests die

> **✅ FIXED 2026-06-17** (worktree `C:/craton/CratonVM-tcfull`, branch
> `tomcat-fullsuite-triage`, `native-builtins/src/jca/provider_chain.rs`). Seeded
> the real Sun PKIX SPI services into the provider chain (same pattern as the
> existing `CertificateFactory X.509` entry):
> `put_service(S, "CertPathValidator", "PKIX", "sun.security.provider.certpath.PKIXCertPathValidator")`
> and `CertPathBuilder PKIX → sun.security.provider.certpath.SunCertPathBuilder`.
> Both Sun SPI classes have the public no-arg ctor `build_jca_instance`'s
> `new_object_initialized(cls,"()V")` needs (verified via `javap` on JDK 25), so
> `CertPathValidator.getInstance("PKIX")` now resolves and runs **real** JDK
> cert-path validation. Purely additive (two service-table entries) → no
> regression risk; real validation is strictly more correct than the prior abort.
>
> **Verification:** the `no CertPathValidator PKIX implementation in any provider`
> abort is **gone from every OCSP log** (0 occurrences). `TestOcspEnabled` now
> runs ~14 JSSE OCSP test-case variants (was: aborted on test case #1).
> **Residual (separate bugs, not DF06):** the OpenSSL-connector variants hang on
> the Apache tomcat-native stub (`incompatible version [2.0.38-cratonvm-stub]` —
> environmental, like HotSpot's openssl ignores), and `TestOcspSoftFail` now hits
> **DF02** (the register-resident JIT-root stale-OOP `Object.hasNext()Z` linkage
> error) once it progresses past PKIX.

**Severity:** Medium (genuine crypto feature gap; process aborts the run).
**Status on CratonVM:** NOSUMMARY (process-death) → **FIXED** (PKIX abort gone). **HotSpot:** PASS.
**Run date:** 2026-06-17
**Binary:** dev `77620f55` (worktree `C:/craton/CratonVM-tcfull`).
**Affected classes (5 NOSUMMARY + 1 HANG):**
`org.apache.tomcat.security.TestSecurity2017Ocsp`,
`org.apache.tomcat.util.net.ocsp.TestOcspSoftFail`,
`org.apache.tomcat.util.net.ocsp.TestOcspSoftFailTryLater`,
`org.apache.tomcat.util.net.ocsp.TestOcspSoftFailInternalError`,
`org.apache.tomcat.util.net.ocsp.TestOcspEnabled`,
`org.apache.tomcat.util.net.ocsp.TestOcspTimeout` (HANG).

## Symptom

The TLS layer attempts PKIX certificate-path validation (OCSP revocation
checking) and the VM aborts with an unimplemented-feature runtime error — no
JUnit summary is produced:

```
INFO [...TestOcspEnabled] Starting test case [test[JSSE with OpenSSL trust false: ...]]
Error in thread "main" runtime error: not implemented: no CertPathValidator PKIX implementation in any provider
```

## Root cause (analysis)

CratonVM exposes no `CertPathValidator` of type `PKIX` from any registered
security provider (`CertPathValidator.getInstance("PKIX")` finds nothing), so
the cert-path / OCSP revocation tests cannot run. This is a deliberate
fail-closed gap in the JCA layer (cf. `reference_jca_synthetic_crypto_layers`),
not a corruption — but it terminates the process rather than throwing a
catchable `NoSuchAlgorithmException`, which is why the run ends as NOSUMMARY
instead of a clean per-test failure.

## Reproduction

```powershell
cd C:\craton\CratonVM\apps\tomcat
$CP = (Get-Content .tooling\cp.txt -Raw).Trim()
$env:CRATONVM_REAL_NET_SOCKETS="1"; $env:CRATONVM_REAL_AQS="1"; $env:CRATONVM_DISABLE_DEFAULT_WATCHDOG="1"
C:\craton\CratonVM-tcfull\target\release\cratonvm.exe -Xmx2g -cp $CP `
  org.junit.runner.JUnitCore org.apache.tomcat.util.net.ocsp.TestOcspEnabled
```

## Recommendation

**HANDOFF (crypto).** Route `CertPathValidator`/`CertPathBuilder` `PKIX` to the
real Sun/BouncyCastle implementation (the same approach used to route RSA/EC
KeyFactory and Cipher to real providers — see the prior PEMFile/JCA work). As a
cheap intermediate, make the missing-algorithm path throw a catchable
`NoSuchAlgorithmException` rather than aborting the VM, so these become clean
FAILs instead of NOSUMMARY process-deaths.
