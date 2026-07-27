# crypto/fips1402: FIPS1402JwtVcMetadataTrustedSdJwtIssuerTest hang after successful provider init — FIXED

Status: fixed on 2026-07-07 (branch `fix/keycloak-fips1402-provec-classname-20260707`) —
same root cause and fix as
[keycloak-crypto-fips1402-provec-algorithmparametersspi-classnotfound-FIXED.md](keycloak-crypto-fips1402-provec-algorithmparametersspi-classnotfound-FIXED.md):
BouncyCastle-FIPS registers algorithm implementations through a private `creatorMap`
(`EngineCreator` factories) that CratonVM's real-JCA bridge couldn't see, so it tried to
reflectively `Class.forName` a cosmetic, non-loadable `className` label instead. For
`AlgorithmParameters.getInstance("EC")`/`Signature` resolution reached from this
particular test's code path, the failure mode was a silent hang rather than the fatal
process-abort seen in the sibling doc — the exact hang mechanism was never fully
root-caused (a retry/blocking path somewhere downstream of the failed resolution), but
since the fix eliminates the bad resolution entirely, the hang is moot.

## Validation

Direct repro via `KcRunner` on the real `crypto/fips1402` classpath
(`../../../../apps/keycloak/crypto/fips1402`, `bc-fips-2.1.2.jar`), JIT on, `--stack-dump-on-timeout 280`:

```
Test run finished after 117250 ms
[ 3 containers found ] [ 3 containers successful ] [ 0 containers failed ]
[ 16 tests found ] [ 16 tests successful ] [ 0 tests failed ]
KCRUNNER_RESULT tests=16 failed=0 aborted=0 skipped=0 containersFailed=0
```

16/16 tests pass in ~117s (dominated by BC-FIPS's own module HMAC self-integrity check
over the ~8.6MB jar, not a hang) — previously this class hit the full 1200s timeout with
zero output after `FIPS1402Provider created`.

---

# Historical report: FIPS1402JwtVcMetadataTrustedSdJwtIssuerTest hangs after successful provider init

Historical original status: open — genuine hang (300s timeout not part of the original
2026-07-04 sweep's evidence for this class)

Date observed: 2026-07-06 (1200s-timeout rerun, branch fix/keycloak-nonpassed-rerun-1200s-20260706)

## Summary

`crypto/fips1402 :: org.keycloak.crypto.fips.test.sdjwt.FIPS1402JwtVcMetadataTrustedSdJwtIssuerTest`
hit the full 1200-second timeout and was killed (`HANG`, `rc=TIMEOUT`,
`1200.128s`). This class was previously part of the 21-class
`crypto/fips1402` group that failed instantly with "Not able to load any
cryptoProvider" (see
`keycloak-crypto-fips1402-cryptoprovider-serviceloader.md`)
— that ServiceLoader gap was already fixed elsewhere on `dev` since
the 2026-07-04 sweep, since this run shows the FIPS provider now initializing
successfully:

```
java.lang.UnsatisfiedLinkError: org/bouncycastle/crypto/fips/VariantSelector.getBestVariantName()Ljava/lang/String;
    at org.bouncycastle.crypto.fips.NativeLoader.loadDriver(Unknown Source)
    at org.bouncycastle.crypto.fips.FipsStatus.isReady(Unknown Source)
    at org.bouncycastle.crypto.CryptoServicesRegistrar.getDefaultMode(Unknown Source)
    at org.bouncycastle.crypto.CryptoServicesRegistrar.<clinit>(Unknown Source)
    ...
    at org.keycloak.crypto.fips.FIPS1402Provider.<init>(FIPS1402Provider.java:91)
    at org.keycloak.common.crypto.CryptoIntegration.detectProvider(CryptoIntegration.java:60)
DEBUG [org.keycloak.crypto.fips.FIPS1402Provider] Could not detect if FIPS is enabled from the host
    java.nio.file.NoSuchFileException
WARN cratonvm_classloading::jar_signer: jar signer: rejecting signer block: SignerInfo is missing authenticatedAttributes — refusing to skip integrity check
INFO [org.keycloak.crypto.fips.FIPS1402Provider] FIPS1402Provider created: KC(BCFIPS version 2.0102, FIPS-JVM: unknown)
```

— then **the log goes completely silent** for the remaining ~1197 seconds
until the harness kills the process on timeout. No further output, no
exception, no partial test progress.

## Notes (original, at filing time)

- The `UnsatisfiedLinkError` for `VariantSelector.getBestVariantName()` is
  **not fatal** here — BouncyCastle FIPS's native-acceleration probe fails
  gracefully (expected: no native BC-FIPS driver is installed in this
  environment) and it falls back to the pure-Java implementation, completing
  provider construction successfully. This line is a red herring for the
  hang itself.
- The actual hang happens **after** `FIPS1402Provider created` — i.e.
  somewhere inside the test class's own SD-JWT / trusted-issuer metadata
  logic. **(Resolved finding: this is the `AlgorithmParameters`/`Signature`
  EC resolution via BC-FIPS's `EngineCreator`/`creatorMap` mechanism — see
  the FIXED sibling doc — not a network/file lookup or a genuine
  deadlock/livelock.)**

## Repro

```
ssh -i "C:\Users\Victor\.ssh\azure.pem" -o IdentitiesOnly=yes victor@20.83.144.174
cd /data/data/wt-keycloak-nonpassed-1200-20260706
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/keycloak-suite-runner/run-keycloak-suite.ps1 -Vm craton \
  -ClassList <(printf 'module\tclass\ncrypto/fips1402\torg.keycloak.crypto.fips.test.sdjwt.FIPS1402JwtVcMetadataTrustedSdJwtIssuerTest\n') \
  -TimeoutSec 120 -RunName repro-fips-sdjwt-hang \
  -KeycloakRoot apps/keycloak-fresh \
  -Exe target/release/cratonvm-nonpassed1200-20260706 -JdkHome /home/victor/jdk25
```

## Evidence

`/data/data/wt-keycloak-nonpassed-1200-20260706/apps/keycloak-suite-runner/.suite/results/nonpassed1200-20260706-shard1/others-jit/logs/crypto_fips1402.org.keycloak.crypto.fips.test.sdjwt.FIPS1402JwtVcMetadataTrustedSdJwtIssuerTest.{out,err}.log` (2026-07-06 4-shard rerun with 1200s timeout, `results.tsv` row: `status=HANG rc=TIMEOUT seconds=1200.128`).
