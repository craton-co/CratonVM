# Keycloak `crypto/fips1402` JUnit assumption aborts — reporting classification FIXED

Status: fixed on 2026-07-15 in `fix/keycloak-fips1402-abort-policy-20260715`.

## Root cause

`run-keycloak-suite.ps1` previously labelled every nonzero JUnit `aborted` count as `FAIL`, alongside actual failed tests and failed containers. That conflated intentional `Assume.assumeTrue(Environment.isJavaInFipsMode())` outcomes with correctness failures.

A non-FIPS host is expected to abort FIPS-gated test methods. The runner now reports:

- `SKIP` when every discovered test was assumption-aborted.
- `PARTIAL` when some tests passed and the remaining tests were assumption-aborted.
- `FAIL` only when KcRunner reports failed tests or failed containers.

## Azure verification

The Linux Azure host `20.83.144.174` is not in host FIPS mode: `/proc/sys/crypto/fips_enabled` is absent. A focused 21-class `crypto/fips1402` run used the same Keycloak class list, JDK 25, classpath, and runner configuration under both VMs.

| VM | PASS | PARTIAL | SKIP | failed | aborted | containersFailed |
|---|---:|---:|---:|---:|---:|---:|
| HotSpot | 11 | 3 | 7 | 0 | 53 | 0 |
| CratonVM (JIT on) | 11 | 3 | 7 | 0 | 53 | 0 |

Every class received the same status under both VMs. The ten FIPS-gated classes comprise seven all-aborted `SKIP` rows and three mixed `PARTIAL` rows; neither is a CratonVM failure.

The CratonVM validation binary was `/data/cratonvm-fips1402-abort-policy-20260715/cratonvm-fips1402-abort-policy-20260715`, built from current `dev` with isolated target, Cargo cache, and temporary directories under `/data/data`.

## Evidence

- HotSpot results: `apps/keycloak-suite-runner/.suite/results/fips1402-hotspot-20260715-v2/hotspot-jit/results.tsv`.
- CratonVM results: `apps/keycloak-suite-runner/.suite/results/fips1402-craton-20260715/all-jit/results.tsv`.
- The ten FIPS-gated rows have `failed=0`; their 53 combined aborts are JUnit assumptions, not failures.

No VM runtime fix was required. This change fixes only the harness reporting policy so non-FIPS environments no longer create false Keycloak failure residuals.
