# HotSpot Baseline - Keycloak Suite

Captured on 2026-07-01 with `apps\keycloak-suite-runner\run-keycloak-suite.ps1`
against the local compiled checkout at `C:\craton\CratonVM\apps\keycloak`.

## Command

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 `
  -Vm hotspot -Category all -RunName hotspot-baseline-20260701-full -Parallel 1 -TimeoutSec 600
```

## Result

| Metric | Value |
|---|---:|
| Classes recorded | 238 |
| Wall seconds | 463.928 |
| Summed class seconds | 456.314 |
| PASS | 145 |
| FAIL | 51 |
| EMPTY | 24 |
| CRASH/NOSUMMARY | 18 |

The non-pass HotSpot outcomes are local classpath/environment baseline outcomes,
not CratonVM-specific findings.

## Files

The runner copied the baseline to:

```text
apps\keycloak-suite-runner\.suite\baseline\hotspot-baseline-hotspot-baseline-20260701-full.tsv
apps\keycloak-suite-runner\.suite\baseline\hotspot-baseline-hotspot-baseline-20260701-full.md
apps\keycloak-suite-runner\.suite\baseline\hotspot-baseline-latest.tsv
apps\keycloak-suite-runner\.suite\baseline\hotspot-baseline-latest.md
```

The full run directory, including per-class stdout/stderr logs, is:

```text
apps\keycloak-suite-runner\.suite\results\hotspot-baseline-20260701-full\hotspot-jit\
```
