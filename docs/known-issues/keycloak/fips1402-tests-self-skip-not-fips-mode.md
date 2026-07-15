# `crypto/fips1402` test classes self-skip (JUnit abort, not fail) when the host isn't running in true FIPS mode — confirmed NOT a CratonVM bug (reproduces identically under real HotSpot)

Status: confirmed non-CratonVM environment behavior — verified via direct HotSpot A/B comparison in this exact
custom harness, not just cited from an older rollup note

Date verified: 2026-07-15 (branch fix/keycloak-nonpassed-rerun-v2-20260710)

## Summary

10 `crypto/fips1402` classes are reported as harness-level "FAIL" by `run-keycloak-suite.ps1`'s status
classifier, which made them look like a correctness cluster at first glance. Reading the actual JUnit output
shows something different: every test in these classes is JUnit-**aborted**, not failed:

```
[         5 tests found           ]
[         0 tests skipped         ]
[         5 tests started         ]
[         5 tests aborted         ]
[         0 tests successful      ]
[         0 tests failed          ]

KCRUNNER_RESULT tests=5 failed=0 aborted=5 skipped=0 containersFailed=0
```

The accompanying debug log line explains why:

```
DEBUG [org.keycloak.crypto.fips.FIPS1402Provider] Could not detect if FIPS is enabled from the host
    java.nio.file.NoSuchFileException
```

`FIPS1402Provider` probes a host-level indicator of true FIPS mode (a Linux-only file, not present on Windows,
hence the caught-and-logged `NoSuchFileException` — this is not the class's actual failure, just an informational
DEBUG line), and each test method's own `Assume.assumeTrue(Environment.isJavaInFipsMode())`-style guard (or
equivalent) then self-skips (JUnit "aborted") since the environment genuinely isn't in FIPS mode.

## Verification

Ran the same class (`org.keycloak.crypto.fips.test.FIPS1402CertificateIdentityExtractorTest`) through the
identical harness under `-Vm hotspot`:

```
[         5 tests found           ]
[         5 tests started         ]
[         5 tests aborted         ]
[         0 tests failed          ]
KCRUNNER_RESULT tests=5 failed=0 aborted=5 skipped=0 containersFailed=0
```

Identical `tests=5 failed=0 aborted=5` outcome under real HotSpot (JDK 25) on the same Windows host. This
confirms the self-skip is a genuine environment property (this Windows dev box isn't running in FIPS mode),
not a CratonVM-specific detection or behavior gap — HotSpot self-skips exactly the same way.

## Disposition

No CratonVM fix needed; this is by-design test behavior for a non-FIPS host. The harness's own "FAIL" status
label for `aborted>0, failed=0` rows is a labeling quirk worth being aware of when triaging future runs (an
"aborted"-only class is not a real failure and shouldn't be investigated as one), but that's a harness/reporting
nit, not a bug in the harness's actual test execution.

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
# module\tclass
# crypto/fips1402\torg.keycloak.crypto.fips.test.FIPS1402CertificateIdentityExtractorTest
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm hotspot -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-fips-hotspot -ClassList <path-to-classlist-above> -KeycloakRoot apps\keycloak -JdkHome $jdk
```
(Swap `-Vm hotspot` for `-Vm craton` with a CratonVM `-Exe` to see the identical abort pattern under CratonVM.)

## Evidence

- CratonVM: `apps/keycloak-suite-runner/.suite/results/nonpassed-v3-shard1/all-jit/logs/crypto_fips1402.*.out.log`
  (2026-07-14 rerun, binary `cratonvm-nonpassed-v3-refresh-20260714.exe`, `dev` commit `e85f76d00`) — all 10
  affected classes: `FIPS1402CertificateIdentityExtractorTest`, `FIPS1402HmacTest`, `FIPS1402JWETest`,
  `FIPS1402JWKTest`, `FIPS1402KeyPairVerifierTest`, `FIPS1402KeystoreTypesTest`,
  `FIPS1402Pbkdf2PasswordPaddingTest`, `FIPS1402SecureRandomTest`, `FIPS1402SslTest`, `PemUtilsBCFIPSTest`.
- HotSpot A/B repro: `apps/keycloak-suite-runner/.suite/results/repro-fips-hotspot/hotspot-jit/logs/*.out.log`
  (2026-07-15, JDK 25).
