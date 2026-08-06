# Spring Boot suite — 46-class non-passed rerun on Windows, 2026-08-06

Reran the 46-class non-passed set from the
[08-05 Azure full-suite run](RESULTS-20260805-azure-fullsuite.md) (39 FAIL
/ 1 BOTH-FAIL / 4 HANG / 2 CRASH) — this time on the **Windows local box**
rather than Azure Linux, after merging `dev` forward and rebuilding.

- **Worktree:** `C:\craton\CratonVM-spring-boot-residual-20260728`, branch
  `feat/spring-boot-residual-rerun-20260728`, merged forward to `origin/dev`
  @ `65a3085f5` (1275 commits ahead of this worktree's previous merge point
  — untouched since 08-01).
- **Binary:** `target\release\cratonvm.exe`. Note the custom bin target
  `cratonvm-spring-boot-residual0728` this worktree previously built no
  longer exists in `Cargo.toml` (renamed/removed upstream at some point in
  the 1275-commit gap) — rebuilt against the generic `cratonvm` bin instead.
- **Sharding:** 7 shards (7/7/7/7/6/6/6 classes), `-Parallel 1` per shard,
  `-TimeoutSec 1500`, each with its own `-RunName`.

## A real bug found and fixed in the runner script before this could run

`run-spring-boot-suite.ps1` failed to parse at all under Windows PowerShell
5.1 (`Unexpected token '$module' in expression or statement` at line 794,
with a cascade of ~15 more bogus errors downstream). Root cause: the file
is UTF-8 **without a BOM**, and a Unicode em-dash (U+2014) in a string
literal at line 384 (`Write-Info "hotspot baseline: none at $path —
..."`). Windows PowerShell 5.1 (unlike `pwsh`, which correctly defaults to
UTF-8) reads a BOM-less `.ps1` using the system codepage, so the em-dash's
3-byte UTF-8 encoding gets misread as garbage characters that desync the
tokenizer for the rest of the file — a clean parse on Linux/`pwsh`, total
parse failure on Windows PS 5.1 (no `pwsh` available on this box). Bisected
with `[System.Management.Automation.Language.Parser]::ParseFile`/
`ParseInput` (the *reported* error location is always downstream of the
real defect once one token desyncs). Fixed by replacing the em-dash with
the plain `--` the rest of the file already uses for the same purpose —
committed separately (`d6fed2e3a`) since it's a real bug affecting any
Windows PS 5.1 user of this shared script, not specific to this rerun.

## Totals

| Status | Count |
|---|---:|
| PASS | 43 (93.5%) |
| HANG | 1 |
| EMPTY | 1 |
| FAIL | 1 |
| **Total** | **46** |

A dramatic improvement from the 08-05 Azure baseline — all 8 classes from
the SSLSocketFactory-double-registration regression, all the
Mockito/ByteBuddy-retransform-NPE classes, all the annotation-metadata-null
classes, and the web-server bootstrap cluster now pass. This isn't
necessarily all attributable to `dev` fixes landing between 08-05 and
08-06 — running on a different OS (Windows vs. Linux) and a different
Spring Boot fixture checkout (this worktree's own `apps/spring-boot`, not
Azure's `springboot-jsonreader-deprecation-20260718`) means this isn't a
clean apples-to-apples regression check; some of the 08-05 Azure findings
could be Linux/fixture-specific and simply not reproduce here. Not
disambiguated this round.

### Residual (3 non-PASS)

| Module | Class | Status | Seconds |
|---|---|---|---:|
| `module/spring-boot-jooq` | `JooqAutoConfigurationTests` | HANG | 1500.0 (full timeout) |
| `module/spring-boot-kafka` | `KafkaAutoConfigurationIntegrationTests` | EMPTY | 1.2 (0 tests ran) |
| `module/spring-boot-pulsar` | `PulsarAutoConfigurationTests` | FAIL | 242.7 |

`JooqAutoConfigurationTests` matches the `jooqautoconfigurationtests-timeout-regression-20260805.md`
doc filed from the Azure investigation. `KafkaAutoConfigurationIntegrationTests`
and (implicitly) the Kafka/Pulsar cluster generally are already being
investigated in a separate concurrent session per explicit instruction this
round — not re-investigated here. The EMPTY status (0 tests selected/ran in
1.2s) is a different symptom than the Azure FAIL and worth flagging to
whoever owns that investigation.

## Reproduce / rerun

```powershell
$env:JAVA_HOME = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
$exe = "<worktree>\target\release\cratonvm.exe"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Exe $exe -JdkHome $env:JAVA_HOME `
  -ClassList apps\spring-boot-suite-runner\.suite\nonpassed-20260805-46.tsv `
  -Start <N> -Count <M> -RunName <name> -Parallel 1 -TimeoutSec 1500
```

Input list: `apps/spring-boot-suite-runner/.suite/nonpassed-20260805-46.tsv`
(46 classes, `module\tclass` header included, extracted from the merged-in
Azure 08-05 full-suite results).

Full results, per shard:
`apps/spring-boot-suite-runner/.suite/results/craton-nonpassed-20260806-s{1..7}/all-jit/results.tsv`.
