# Four Spring Boot classes are red because of the JIT-triggered `[moving-young]` fallback, not because of the 300s budget

**Status: OPEN — measured 2026-08-10. Supersedes the "margin exhausted",
"recurring timeout" and "cumulative `ModifiedClassPathExtension` cost" framings
of the classes below, whose own docs are retired to
`docs/internal/fixed-suite-bugs/springboot/`.**

> **A 300s log is too short to see this.** `Log4J2LoggingSystemTests` was first
> read as *not* an instance of this bug, precisely because its 300s suite log
> contains **zero** fallback lines. Given a 3600s budget the same class reaches
> **#4096** and still never finishes. The escalation outlasts the budget that
> kills the process, so absence of the warning in a timed-out log is not
> evidence of absence — re-run with a real budget before ruling a class out.

## The measurement

Standalone, one class at a time (no shard concurrency), `--Xmx 2g`, from the
`CratonVM-spring-boot-residual-20260728` checkout. Binaries
`cratonvm-flymargin-20260810.exe` (dev `f695ca875`) and
`cratonvm-logsys-20260810.exe` (dev `6365de194`). HotSpot control is Temurin
25.0.3 on the same host, same classpath, same runner.

| Class | HotSpot | CratonVM, JIT on | CratonVM `--nojit` | `[moving-young]` fallback peak |
|---|---:|---|---:|---:|
| `…flyway.autoconfigure.FlywayAutoConfigurationTests` | 10.0s ✓73/73 | 496.2s ✓73/73 | **218.5s** ✓73/73 | 7 |
| `…integration.autoconfigure.IntegrationAutoConfigurationTests` | 11.2s ✓34/34 | **OOM after 3.8 hours** | **221.9s** ✓34/34 | **#16384** |
| `…quartz.actuate.endpoint.QuartzEndpointWebIntegrationTests` | 12.6s ✓45/45 | **no completion in 2400s** | **262.9s** (see note) | **#4096** |
| `…logging.log4j2.Log4J2LoggingSystemTests` | 10.7s (61, 14 fail) | **no completion in 3600s** | **293.3s** (61, 14 fail) | **#4096** |

**With the JIT off, all four finish in 218–293s — inside the default 300s
budget — and log zero fallbacks.** With the JIT on, one takes 2.3x longer, one
dies of heap exhaustion, and two do not finish in 8–12x their budget.

`Log4J2LoggingSystemTests`'s 14 failures are identical under HotSpot and
CratonVM `--nojit`, so they are pre-existing/environmental and not part of this
defect. Its sibling `LogbackLoggingSystemTests` is **not** affected: it
completes both ways (268.2s JIT / 277.5s `--nojit`, 86/86 both) with **zero**
fallbacks, and is simply ~30x HotSpot — see its retired doc.

Note on Quartz `--nojit`: it reports 29/45 failed, but every failure is
`ApplicationContextException: Failed to start bean 'webServerStartStop'` from
`Connector["http-nio-8080"]` failing to bind — port 8080 was held by an
unrelated process on this shared host. That is environmental and does not
affect the timing result. The tests that bind port 0 pass.

## Why this is not a timeout-budget problem

The retired docs recommend the same fix: raise these classes' per-class timeout
(`slowClasses` in `run-spring-boot-suite.ps1`). The measurements say that is
wrong on both counts:

- For `Integration`, `Quartz` and `Log4J2` it **would not work**. They do not run
  long and then succeed; they fail. `Integration` OOMs — a longer budget moves
  the failure later, it does not produce a pass. `Quartz` had not finished at
  2400s and `Log4J2` had not finished at 3600s, 8–12x the budget they are
  alleged to be marginally over.
- For all four it **would hide the defect**. The same classes complete
  comfortably inside the existing 300s budget with the JIT disabled. Nothing
  about these classes is intrinsically too slow for the budget.

No `slowClasses` entries were added.

## Mechanism, and why it is already known

`[moving-young] fallback` means a young collection could not use the moving
(compacting) collector and fell back to the non-moving sweep. Two reasons appear
here — `unregistered-jit-frame-on-stack` (Flyway, Integration) and
`innermost-rbp-belongs-to-unguarded-callee` (Quartz, Log4J2) — both raised by
the conservative stack scan when it cannot prove a JIT frame is safe to move.

Once the process is stuck in that mode, the young generation is swept but never
compacted, so its free list fragments monotonically. That end state is already
documented: a non-moving young sweep decays until the largest hole is a few KB
while the generation reads ~98% free. Allocation then fails with an
`OutOfMemoryError` naming a small object while most of the heap is idle —
exactly the `Integration` failure here, and exactly the previously-diagnosed
`ZipContentTests.nestedZip64CanBeRead` failure, which ran **512** consecutive
`reason=unregistered-jit-frame-on-stack` fallbacks, could not allocate a
`byte[8192]` with 1042 MB of old-generation headroom, and passed 29/29 under
`--nojit` (see the `jit_newarray` comment in `vm/src/jit/helpers.rs` and the A5
notes in `vm/src/jit/conservative_roots.rs`).

The A5 return-address filter (`is_plausible_return_pc`, 2026-08-04) narrowed the
false-positive source that caused the `ZipContentTests` case. **It does not cover
these four**: peaks of #4096 and #16384 are far past the 512 that filter was
built for. Whether the remaining fallbacks are further false positives or
genuine unregistered frames is not established here.

## The count is the triage signal

The fallback peak separates the two outcomes cleanly, and cheaply:

- **single digits → harmless.** Flyway hits 7 and still passes 73/73;
  `LogbackLoggingSystemTests` logs none at all and passes 86/86.
- **thousands → death spiral.** Integration #16384 → OOM; Quartz and Log4J2
  #4096 → no completion.

…with the caveat in the banner above: the count only reaches those thousands if
the process is allowed to run past the budget that would normally kill it.

`run-spring-boot-suite.ps1` now records `moving-young-fallback peak=#N <reason>`
in the `note` column of every row that logs one, so a future `HANG` shows the
escalation at a glance rather than costing a multi-hour standalone rerun to
discover. Validated against all five logs from this session.

## What is not established

- **Which fallbacks are false positives.** No attempt was made here to attribute
  individual fallbacks to real vs conservatively-misread frames.
- **Whether `Quartz` also has a genuine stall.** Its retired doc raised an
  intermittent Netty/Tomcat lifecycle stall as a second candidate. A single
  non-completing JIT run and a single completing `--nojit` run do not exclude
  it; they only show the fallback is sufficient to explain the observed rows.
- **Whether `--nojit` is a fix.** It is a diagnostic lever, not a remedy — it
  removes the trigger by removing the JIT.
- **Host load.** Four unrelated CratonVM processes from other sessions were
  running during these measurements. Absolute wall-clock is therefore an upper
  bound; the JIT-vs-`--nojit` contrasts (2.3x, OOM-vs-pass, no-finish-vs-pass)
  are far too large to be explained by it.

## Reproduce

```bash
CV=target/release/cratonvm.exe
CP="$(cat module/spring-boot-integration/build/cratonvm-test-cp.txt);<sb>/sb-runner"
# dies with OutOfMemoryError, fallback peak #16384
"$CV" --Xmx 2g            --cp "$CP" SbRunner org.springframework.boot.integration.autoconfigure.IntegrationAutoConfigurationTests
# passes 34/34 in ~222s, zero fallbacks
"$CV" --Xmx 2g --nojit    --cp "$CP" SbRunner org.springframework.boot.integration.autoconfigure.IntegrationAutoConfigurationTests
```

## Related

- `docs/known-issues/h2/h2-update-path-throughput-20260802.md` — names the
  non-moving fallback as a scaling target on an unrelated workload; same
  mechanism seen as throughput rather than as failure.
- `docs/internal/fixed-suite-bugs/springboot/flyway-integration-300s-margin-RETIRED-20260810.md`
- `docs/internal/fixed-suite-bugs/springboot/quartzendpointwebintegrationtests-recurring-timeout-RETIRED-20260810.md`
- `docs/internal/fixed-suite-bugs/springboot/log4j2-logback-loggingsystemtests-RETIRED-20260810.md`

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-flyway` | `org.springframework.boot.flyway.autoconfigure.FlywayAutoConfigurationTests` |
| `module/spring-boot-integration` | `org.springframework.boot.integration.autoconfigure.IntegrationAutoConfigurationTests` |
| `module/spring-boot-quartz` | `org.springframework.boot.quartz.actuate.endpoint.QuartzEndpointWebIntegrationTests` |
| `core/spring-boot` | `org.springframework.boot.logging.log4j2.Log4J2LoggingSystemTests` |
