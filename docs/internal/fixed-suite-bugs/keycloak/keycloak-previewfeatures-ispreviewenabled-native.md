# Keycloak non-passed rerun: missing PreviewFeatures.isPreviewEnabled native

## Status: FIXED (2026-07-03)

Independently re-discovered the same day while reproducing the unrelated
`http.client` bug cluster on this same Azure host/JDK 21 (see
`docs/known-issues/http-client-cluster-redefine-dispatch-and-jdk21-gaps.md`) —
every one of that cluster's 12 target classes hit this exact
`UnsatisfiedLinkError` before a single test method could run. No functional
change needed there once this fix was already in place; noted here only for
cross-reference.

Added a native implementation for `jdk/internal/misc/PreviewFeatures.isPreviewEnabled()Z`
in `../../../../native-builtins/src/lib.rs` (`register_essential_natives`, next to the
`jdk/internal/misc/CDS` block), returning `false` — matching HotSpot's
no-`--enable-preview` default. CratonVM does not parse `--enable-preview` yet
(tracked separately in the roadmap), so this only covers the default case;
a true preview-enabled launch would need that flag wired through first.

Verified on the local Windows build (JDK 25.0.1, since `Class.isUnnamedClass()`
from the original probe is JDK21-preview-era API no longer present on JDK 25 —
`jdk.internal.misc.PreviewFeatures.isEnabled()` was used instead via reflection,
which still exercises the same `<clinit>` → `isPreviewEnabled()` native call):

- Pre-fix binary: reproduces the exact reported crash —
  `UnsatisfiedLinkError: jdk/internal/misc/PreviewFeatures.isPreviewEnabled()Z`.
- Post-fix binary: returns `false`, matching HotSpot, `main-vm run()` exits `Ok`.

## Full Azure rerun after fix

Smoke run first, then full suite rerun, both on the Azure host
`victor@20.84.156.31`.

Smoke probe:

- Worktree: `/home/victor/wt-keycloak-previewfeatures-suite-20260703-01`
- Branch: `codex/keycloak-previewfeatures-suite-20260703-01`
- Branch head: `d57bb9049`
- Binary: `target/release/cratonvm-keycloak-previewfeatures-suite-20260703-01`
- Probe: `PreviewFeaturesProbe.class.isUnnamedClass()` on JDK 21
- HotSpot: `rc=0`, output `false`
- CratonVM: `rc=0`, output `false`, `main-vm run()` exited normally

Full rerun:

- Run name: `craton-azure-nonpassed-dev-20260703-previewfeatures-fixed-01`
- Mode: `others-jit`, `-Vm craton`, `-Jit on`
- Class list: 1044 classes from the previous non-passed Keycloak batch
- Results: `apps/keycloak-suite-runner/.suite/results/craton-azure-nonpassed-dev-20260703-previewfeatures-fixed-01/others-jit/results.tsv`
- Summary: `apps/keycloak-suite-runner/.suite/results/craton-azure-nonpassed-dev-20260703-previewfeatures-fixed-01/others-jit/summary.md`
- Wall seconds: `1526.233`

Status count after the fix:

| Status | Count |
|---|---:|
| CRASH | 941 |
| FAIL | 66 |
| EMPTY | 37 |

The rerun has zero `PreviewFeatures.isPreviewEnabled` occurrences in
`results.tsv` notes and zero occurrences in per-class stderr logs. The original
1044/1044 launcher-wide native crash is fixed. The remaining rows are later
post-fix residuals, tracked separately in `../../../known-issues`:

| Residual signature | Count |
|---|---:|
| `java/lang/System$1.defineClass(...ProtectionDomain;String;)Class` `NoSuchMethodError` | 621 |
| `LogBuildTimeConfig$$CMImpl` generated config class `no class def found` | 281 |
| `cratonvm/synthetic/AnonymousObject$1.anyMatch(IntPredicate)Z` `NoSuchMethodError` | 64 |
| `org/keycloak/testsuite/model/KeycloakModelTest` `no class def found` | 37 |
| `TestConfig$$CMImpl` generated config class `no class def found` | 2 |
| `java/lang/System$1.findBootstrapClassOrNull(String)Class` `NoSuchMethodError` | 2 |
| Abstract/no-test rows recorded as `EMPTY` | 37 |

Original report follows.

## Symptom

Reproduced on 2026-07-03 on the Azure host
`victor@20.84.156.31`, remote worktree
`/home/victor/wt-keycloak-azure-nonpassed-20260703-01`, branch
`codex/keycloak-azure-nonpassed-20260703-01`, branch head `819948842`.

A rerun of the 1044 Keycloak classes that were non-passing in the prior
`others` run completed with 1044 crashes and no passes/failures:

- Run name: `craton-azure-nonpassed-dev-20260703-04`
- Mode: `others-jit`, `-Vm craton`, `-Jit on`
- CratonVM binary: `target/release/cratonvm-keycloak-azure-nonpassed-20260703-01`
- Results: `apps/keycloak-suite-runner/.suite/results/craton-azure-nonpassed-dev-20260703-04/others-jit/results.tsv`
- Summary: `apps/keycloak-suite-runner/.suite/results/craton-azure-nonpassed-dev-20260703-04/others-jit/summary.md`

Status count:

| Status | Count |
|---|---:|
| CRASH | 1044 |

Module count:

| Module | Count |
|---|---:|
| `tests/base` | 375 |
| `tests/clustering` | 2 |
| `tests/webauthn` | 5 |
| `testsuite/integration-arquillian/tests/base` | 621 |
| `testsuite/integration-arquillian/tests/other/sssd` | 3 |
| `testsuite/model` | 38 |

The result notes split into two top-level shapes:

| Top-level note | Count |
|---|---:|
| `java/lang/UnsatisfiedLinkError: jdk/internal/misc/PreviewFeatures.isPreviewEnabled()Z` | 423 |
| `org/junit/platform/commons/JUnitException: TestEngine with ID 'junit-jupiter' failed to discover tests` | 621 |

The second shape is only a JUnit wrapper. Its stderr contains the same root cause:

```text
Caused by: java/lang/UnsatisfiedLinkError: jdk/internal/misc/PreviewFeatures.isPreviewEnabled()Z
    at java/lang/Class.isUnnamedClass(Class.java:1903)
    at jdk/internal/misc/PreviewFeatures.<clinit>(PreviewFeatures.java:31)
```

The direct crashes show the same missing native earlier in launcher setup:

```text
Missing native method in real-JDK mode method=jdk/internal/misc/PreviewFeatures.isPreviewEnabled()Z
[cratonvm] main-vm run() returned Err: Exception in thread "main" java/lang/UnsatisfiedLinkError: jdk/internal/misc/PreviewFeatures.isPreviewEnabled()Z
    at java/lang/Class.isUnnamedClass(Class.java:1903)
    at jdk/internal/misc/PreviewFeatures.<clinit>(PreviewFeatures.java:31)
```

## Minimal probe

The failure is not Keycloak-specific. A small probe that calls the reflective
preview API succeeds on HotSpot and fails on CratonVM.

```java
public class PreviewFeaturesProbe {
    public static void main(String[] args) {
        System.out.println(PreviewFeaturesProbe.class.isUnnamedClass());
    }
}
```

Commands run on the Azure host:

```bash
PROBE_DIR=/tmp/craton-previewfeatures-probe-20260703
/usr/lib/jvm/java-21-openjdk-amd64/bin/javac -d "$PROBE_DIR" "$PROBE_DIR/PreviewFeaturesProbe.java"
/usr/lib/jvm/java-21-openjdk-amd64/bin/java -cp "$PROBE_DIR" PreviewFeaturesProbe
/home/victor/wt-keycloak-azure-nonpassed-20260703-01/target/release/cratonvm-keycloak-azure-nonpassed-20260703-01 \
  --java-home /usr/lib/jvm/java-21-openjdk-amd64 \
  -cp "$PROBE_DIR" PreviewFeaturesProbe
```

Observed output:

```text
hotspot_rc=0
false
craton_rc=1
Missing native method in real-JDK mode method=jdk/internal/misc/PreviewFeatures.isPreviewEnabled()Z
[cratonvm] main-vm run() returned Err: Exception in thread "main" java/lang/UnsatisfiedLinkError: jdk/internal/misc/PreviewFeatures.isPreviewEnabled()Z
    at PreviewFeaturesProbe.main(PreviewFeaturesProbe.java:3)
    at java/lang/Class.isUnnamedClass(Class.java:1903)
    at jdk/internal/misc/PreviewFeatures.<clinit>(PreviewFeatures.java:31)
```

## Current conclusion

CratonVM's real-JDK native surface is missing
`jdk/internal/misc/PreviewFeatures.isPreviewEnabled()Z`. JUnit calls into
`Class.isUnnamedClass()` during launcher/session setup or discovery; that
initializes `jdk.internal.misc.PreviewFeatures`, which immediately calls the
missing native and aborts the process before any Keycloak tests can execute.

This explains the large crash count: the same launcher/discovery path is shared
by every class in the rerun, so a single missing native collapses the full
1044-class non-passed batch.

## Fix direction

Add a native implementation for
`jdk/internal/misc/PreviewFeatures.isPreviewEnabled()Z` in real-JDK mode. It
should mirror HotSpot's preview-enabled state. The no-preview default should
return `false`; `--enable-preview` behavior should be checked separately so this
does not silently hard-code the wrong value for preview-enabled launches.

After the native is added, rerun the same class list with:

```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/keycloak-suite-runner/run-keycloak-suite.ps1 \
  -ClassList apps/keycloak-suite-runner/.suite/nonpassed-from-craton-others-20260702-01.tsv \
  -Category others -Vm craton -Jit on -Start 1 -Count 0 -Parallel 2 -TimeoutSec 600 \
  -RunName craton-azure-nonpassed-dev-20260703-previewfeatures-fixed \
  -KeycloakRoot apps/keycloak \
  -WorkDir apps/keycloak-suite-runner/.suite \
  -Exe target/release/cratonvm-keycloak-azure-nonpassed-20260703-01 \
  -JdkHome /usr/lib/jvm/java-21-openjdk-amd64
```

## Run hygiene notes

Two runner/environment setup defects were found before the valid run and should
not be counted as CratonVM suite bugs:

- PowerShell on Linux cast `/home/...` paths to relative `System.Uri` values,
  producing blank pathing-JAR manifest classpaths.
- CratonVM did not load `KcRunner` through the pathing-JAR manifest classpath,
  so the runner now uses direct `-cp` on non-Windows systems.

The final evidence run above is `craton-azure-nonpassed-dev-20260703-04`.
Earlier runs `01` and `02` are invalid setup attempts and should be ignored.
Run `03` was the first valid run and produced the same 1044-crash breakdown,
but run `04` is the fresh rerun after fixing the local doc link.
