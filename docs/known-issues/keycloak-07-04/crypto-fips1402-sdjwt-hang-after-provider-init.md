# crypto/fips1402: FIPS1402JwtVcMetadataTrustedSdJwtIssuerTest hangs after successful provider init

Status: open — genuine hang (300s timeout not part of the original 2026-07-04 sweep's evidence for this class)

Date observed: 2026-07-06 (1200s-timeout rerun, branch fix/keycloak-nonpassed-rerun-1200s-20260706)

## Summary

`crypto/fips1402 :: org.keycloak.crypto.fips.test.sdjwt.FIPS1402JwtVcMetadataTrustedSdJwtIssuerTest`
hit the full 1200-second timeout and was killed (`HANG`, `rc=TIMEOUT`,
`1200.128s`). This class was previously part of the 21-class
`crypto/fips1402` group that failed instantly with "Not able to load any
cryptoProvider" (see
`docs/known-issues/keycloak-07-04/crypto-fips1402-cryptoprovider-serviceloader-empty.md`)
— that ServiceLoader gap appears to have been fixed elsewhere on `dev` since
the 2026-07-04 sweep (same pattern observed for the Infinispan
`isClustered()` fix noted in the sibling Liquibase-scope doc), since this run
shows the FIPS provider now initializing successfully:

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

## Notes

- The `UnsatisfiedLinkError` for `VariantSelector.getBestVariantName()` is
  **not fatal** here — BouncyCastle FIPS's native-acceleration probe fails
  gracefully (expected: no native BC-FIPS driver is installed in this
  environment) and it falls back to the pure-Java implementation, completing
  provider construction successfully. This line is a red herring for the
  hang itself, though worth independently confirming CratonVM's handling of
  this particular native-probe pattern isn't *also* masking a different
  problem.
- The actual hang happens **after** `FIPS1402Provider created` — i.e.
  somewhere inside the test class's own SD-JWT / trusted-issuer metadata
  logic (`FIPS1402JwtVcMetadataTrustedSdJwtIssuerTest`), not during crypto
  provider bootstrap. Given the class name, this test likely does an HTTP or
  file-based lookup of "trusted issuer" metadata for SD-JWT verification —
  a plausible hang site is a network call with no timeout (if the test tries
  to reach an external/mock endpoint that isn't available in this harness)
  or a genuine CratonVM concurrency issue (deadlock/livelock) in the
  crypto/JWT verification path.
- Sibling classes in the same `sdjwt` package (`FIPS1402SdJwsTest`,
  `FIPS1402SdJwtCreationAndSigningTest`, `FIPS1402SdJwtKeyBindingTest`,
  `FIPS1402SdJwtVerificationTest`, `FIPS1402SdJwtPresentationConsumerTest`,
  `FIPS1402SdJwtVPTest`, `FIPS1402SdJwtVPVerificationTest`) were not
  confirmed hanging in this same run (their status wasn't captured before
  the host's shared resource pressure killed this rerun's shard processes) —
  worth checking whether the hang is specific to the "TrustedSdJwtIssuer"
  variant (which sounds like it uniquely does an issuer-trust lookup) or
  common to the whole `sdjwt` package.

## Next steps

1. Re-run this single class in isolation with a moderate timeout (60-120s)
   and `RUST_LOG=debug` or a thread-dump-on-timeout mechanism to see exactly
   where the main thread is parked when it hangs (a Java-level stack dump, if
   the harness/VM supports one via signal, would directly show the hung
   frame).
2. Check whether this test requires network access (e.g. to a
   `.well-known` endpoint for issuer trust metadata) that isn't available in
   this sandboxed environment — if so, this could be an environment gap
   rather than a CratonVM bug (analogous to the Maven-artifact-resolution gap
   found in the Charset/MemorySize investigation). Read the test source
   (`crypto/fips1402/src/test/java/org/keycloak/crypto/fips/test/sdjwt/FIPS1402JwtVcMetadataTrustedSdJwtIssuerTest.java`)
   to see what it actually does before assuming a VM-level concurrency bug.
3. Confirm whether real HotSpot also hangs on this same class in the same
   harness (not yet checked) — needed to establish this as a genuine
   CratonVM-vs-HotSpot divergence rather than a shared environment limitation
   (note: this host was under severe CPU/memory pressure from other
   concurrent sessions while this run executed, which is a confound worth
   ruling out with a clean re-run before concluding this is 100% CratonVM's
   fault).

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
