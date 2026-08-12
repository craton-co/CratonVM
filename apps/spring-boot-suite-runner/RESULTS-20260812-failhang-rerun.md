# Spring Boot suite — failed/hung class rerun, 2026-08-12

Rerun of the last known non-passing classes from this branch's own work
([RESULTS-20260806-nonpassed46-windows.md](RESULTS-20260806-nonpassed46-windows.md)'s
3-class residual) plus the two web-server bootstrap classes this branch's
`14f3eb6e0` (jar-accessor `mtime` cache fix) explicitly measured, with a
much longer per-class timeout (**3000s**, up from 1500s) to settle
`JooqAutoConfigurationTests`'s HANG-at-full-timeout status definitively.

- **Where:** local Windows box (Azure host `20.83.144.174` was unreachable —
  TCP-level connection timeout, not the usual banner-exchange overload
  pattern — for 15+ min; switched to local per explicit instruction).
- **Branch state:** `fix/sb-pulsar-hang-tomcat-throughput-20260811` merged
  forward to `origin/dev` (312 commits) immediately before this run.
- **Worktree used for the actual run:** `CratonVM-spring-boot-residual-20260728`
  (already had the Spring Boot fixture + generated per-module classpaths;
  this branch's own worktree did not have the fixture staged) — merged to the
  same `dev` tip and rebuilt there first. Results copied back here.
- **Binary:** freshly rebuilt `target\release\cratonvm.exe`, JDK 25
  (`Eclipse Adoptium jdk-25.0.3.9-hotspot`), `-Parallel 2 -TimeoutSec 3000`.

## Result: 4 PASS, 1 EMPTY (not a hang, not a VM defect) — zero failures

| Module | Class | Status | Seconds | Tests | Failed |
|---|---|---|---:|---:|---:|
| `module/spring-boot-kafka` | `KafkaAutoConfigurationIntegrationTests` | EMPTY | 6.9 | 0 (3 skipped) | 0 |
| `module/spring-boot-jooq` | `JooqAutoConfigurationTests` | **PASS** | 183.1 | 17 | 0 |
| `module/spring-boot-pulsar` | `PulsarAutoConfigurationTests` | **PASS** | 414.2 | 74 | 0 |
| `module/spring-boot-tomcat` | `TomcatServletWebServerFactoryTests` | **PASS** | 554.1 | 132 | 0 |
| `module/spring-boot-jetty` | `JettyServletWebServerFactoryTests` | **PASS** | 510.2 | 113 | 0 |

Wall time 932s for all 5 (parallel=2).

**`JooqAutoConfigurationTests` is no longer a hang.** It passed in 183s —
comfortably under even the OLD 1500s timeout, let alone the new 3000s one —
so whatever the 08-05/08-06 HANG was, it is gone now (most likely one of the
many `dev` fixes landed between 08-06 and this branch's merge point, though
not disambiguated further here; this rerun's job was to confirm the CURRENT
state, not re-attribute the historical one).

`Pulsar`/`Tomcat`/`Jetty` all reconfirm this branch's own fix
(74/74, 132/132, 113/113 respectively — exact match to the counts `14f3eb6e0`
already measured).

**`KafkaAutoConfigurationIntegrationTests`'s EMPTY is a different, already-
flagged issue, not a hang and not this branch's concern.** All 3 discovered
tests are JUnit-**skipped** (not aborted, not failed) — `[3 tests found] [3
tests skipped] [0 tests started]` — the shape of an environment-gated
(likely Testcontainers/Docker) conditional disable, not a VM defect. Per
[RESULTS-20260806-nonpassed46-windows.md](RESULTS-20260806-nonpassed46-windows.md):
"the Kafka/Pulsar cluster generally are already being investigated in a
separate concurrent session per explicit instruction" — left alone here for
the same reason.

## Reproduce

```powershell
$env:JAVA_HOME = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -JdkHome $env:JAVA_HOME `
  -ClassList apps\spring-boot-suite-runner\.suite\rerun-20260812-failhang.tsv `
  -RunName craton-rerun-failhang-20260812 -Parallel 2 -TimeoutSec 3000
```

Input list: `apps/spring-boot-suite-runner/.suite/rerun-20260812-failhang.tsv`.
Full results: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-failhang-20260812/all-jit/results.tsv`.
