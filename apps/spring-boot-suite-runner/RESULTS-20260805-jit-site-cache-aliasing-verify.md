# Spring Boot suite — verifying the JIT site-cache aliasing fix, 2026-08-05

Targeted verification of the classes the 2026-08-05 Azure full-suite run left
open with `java.lang.classfile`-shaped corruption, after `383e7f5cf`
(*a recycled `JitInvokeInfo` address let one call site serve another's
dispatch*) landed on `dev` at 13:22 -0300 — **after** that full-suite run's
binary was cut.

Not a suite rerun: five specific classes, one process each, plus HotSpot
controls.

## Binaries

| Arm | Where | Commit |
|---|---|---|
| pre-fix craton | local Windows worktree | `dev` @ `4192dec7c` (no `383e7f5cf`) |
| fixed craton | local Windows worktree | `dev` @ `a3f76ae64` (has `383e7f5cf`) |
| fixed craton | Azure `/data/data/wt-flywayfix-20260805` | `origin/dev` @ `2396b3685` |
| HotSpot control | Temurin 25.0.3+9 | — |

## Result: all five classes green

| Module | Class | 08-05 full suite | Fixed binary |
|---|---|---|---|
| `module/spring-boot-flyway` | `FlywayAutoConfigurationTests` | **HANG** 300.1s | **PASS 73/73** |
| `module/spring-boot-webmvc` | `WebMvcAutoConfigurationTests` | 88/93 FAIL | **PASS 93/93** |
| `module/spring-boot-webmvc` | `WebMvcObservationAutoConfigurationTests` | 12/13 FAIL | **PASS 13/13** |
| `module/spring-boot-web-server` | `ServletComponentScanIntegrationTests` | 2/3 FAIL | **PASS 3/3** |
| `module/spring-boot-webtestclient` | `WebTestClientAutoConfigurationTests` | 3/11 FAIL | **PASS 11/11** |

## `FlywayAutoConfigurationTests` in detail

Local (Windows), `--Xmx 2g`, runner env vars set:

| Arm | Result | Wall |
|---|---|---:|
| pre-fix, default JIT | 39/73 FAIL | 245.6s |
| pre-fix, `--nojit` | 73/73 PASS | 250.5s |
| pre-fix, `--Xmx 8g` | 44/73 FAIL | 227.9s |
| pre-fix, `CRATONVM_ROOTSNAP_CACHE=0` + `…_SURVIVE_GC=0` | 35/73 FAIL | 420.5s |
| pre-fix, `CRATONVM_JIT_DENY=jdk/internal/constant/` | 69/73 FAIL | 24.4s |
| **fixed, default JIT** | **73/73 PASS** | 214.1s / 220.9s |
| HotSpot control | 73/73 PASS | 9.3s / 11.4s |

Azure Linux, `/data/sbrun.sh`, `--Xmx 4g`, taken at `load average: 79` on the
16-core host:

```
[1/2] jit FlywayAutoConfigurationTests rc=0 PASS 224s tests=73 failed=0 aborted=0 containersFailed=0
[2/2] jit FlywayAutoConfigurationTests rc=0 PASS 232s tests=73 failed=0 aborted=0 containersFailed=0
[1/2] hotspot FlywayAutoConfigurationTests rc=0      tests=73 failed=0 aborted=0 containersFailed=0
[2/2] hotspot FlywayAutoConfigurationTests rc=0      tests=73 failed=0 aborted=0 containersFailed=0
```

224-232s against the recorded `craton-fullsuite-azure-20260802` time of
**220.799s** for the same class: the fix restores the pre-regression speed
exactly, it does not merely make the class finish. HotSpot's baseline row is
27.780s, so this class was ~8x HotSpot both before and after — the 300s ceiling
is close for reasons that predate this bug (73 full `ApplicationContext`
refreshes) and are not a residual of it.

## Negative control: `JooqAutoConfigurationTests` is NOT closed by this

The 08-05 full suite reported four HANGs. `JooqAutoConfigurationTests` was one
of them, and it is a different defect — on the same fixed binary, same Azure
host, same launcher, with a 900s ceiling instead of 300s:

```
[1/1] jit JooqAutoConfigurationTests rc=124 HANG 900s none
```

It reached `HikariPool-11` of 17 in 900s (~60s per `@Test`/pool cycle, against
~90-100s on the pre-fix binary), so this fix speeds it up without bringing it
near the budget. `docs/known-issues/springboot/jooqautoconfigurationtests-timeout-regression-20260805.md`
stays OPEN and still owns it.

This is also what says the four green rows above are the fix and not a quieter
box: the same binary, on the same host, in the same session, still hangs the
class that has an unrelated cause.

## Docs retired

- `docs/known-issues/springboot/flywayautoconfigurationtests-silent-hang-after-hsqldb-validate-20260805.md`
  → `fixed-suite-bugs/springboot/flywayautoconfigurationtests-timeout-jit-site-cache-aliasing-FIXED-20260805.md`
- `docs/known-issues/springboot/classfile-annotation-metadata-corruption-20260805.md`
  → `fixed-suite-bugs/springboot/classfile-annotation-metadata-corruption-FIXED-20260805.md`

Both filed hypotheses were wrong and are recorded as such on the retired
pages: the flyway page blamed a `org/hsqldb/` JIT ban that had already been
deleted on 08-01, and the classfile page blamed CratonVM serving corrupt
`.class` bytes (a probe reading the identical resource and parsing it 192k
times passes on the *pre-fix* binary).

## Reproduce

```bash
# Azure
ssh -i ~/.ssh/azure.pem victor@20.83.144.174
/data/sbrun.sh /data/data/wt-flywayfix-20260805/target/release/cratonvm-flywayfix \
  jit module/spring-boot-flyway \
  org.springframework.boot.flyway.autoconfigure.FlywayAutoConfigurationTests \
  /tmp/flywayfix-out 2 900
/data/hsrun.sh module/spring-boot-flyway \
  org.springframework.boot.flyway.autoconfigure.FlywayAutoConfigurationTests \
  /tmp/flywayfix-hs 2
```

```powershell
# Windows
apps\spring-boot-suite-runner\run-single-class.ps1 `
  -Module module/spring-boot-flyway `
  -ClassName org.springframework.boot.flyway.autoconfigure.FlywayAutoConfigurationTests `
  -Exe <path-to-cratonvm.exe>
```
