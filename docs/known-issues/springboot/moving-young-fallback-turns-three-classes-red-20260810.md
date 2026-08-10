# Three Spring Boot classes are red because of the JIT-triggered `[moving-young]` fallback, not because of the 300s budget

**Status: OPEN — measured 2026-08-10. Supersedes the "margin exhausted" and
"recurring timeout" framings of the three classes below, whose own docs are
retired to `docs/internal/fixed-suite-bugs/springboot/`.**

## The measurement

Standalone, one class at a time (no shard concurrency), `--Xmx 2g`, from the
`CratonVM-spring-boot-residual-20260728` checkout, binary
`cratonvm-flymargin-20260810.exe` (release build of dev `f695ca875`). HotSpot
control is Temurin 25.0.3 on the same host, same classpath, same runner.

| Class | HotSpot | CratonVM, JIT on | CratonVM `--nojit` | `[moving-young]` fallback peak |
|---|---:|---|---:|---:|
| `…flyway.autoconfigure.FlywayAutoConfigurationTests` | 10.0s ✓73/73 | 496.2s ✓73/73 | **218.5s** ✓73/73 | 7 |
| `…integration.autoconfigure.IntegrationAutoConfigurationTests` | 11.2s ✓34/34 | **OOM after 3.8 hours** | **221.9s** ✓34/34 | **#16384** |
| `…quartz.actuate.endpoint.QuartzEndpointWebIntegrationTests` | 12.6s ✓45/45 | **no completion in 2400s** | **262.9s** (see note) | **#4096** |

**With the JIT off, all three finish in 218–263s — inside the default 300s
budget — and log zero fallbacks.** With the JIT on, one takes 2.3x longer, one
dies of heap exhaustion, and one does not finish in 8x its budget.

Note on Quartz `--nojit`: it reports 29/45 failed, but every failure is
`ApplicationContextException: Failed to start bean 'webServerStartStop'` from
`Connector["http-nio-8080"]` failing to bind — port 8080 was held by an
unrelated process on this shared host. That is environmental and does not
affect the timing result. The tests that bind port 0 pass.

## Why this is not a timeout-budget problem

Both retired docs recommend the same fix: raise these classes' per-class
timeout (`slowClasses` in `run-spring-boot-suite.ps1`). The measurements say
that is wrong on both counts:

- For `Integration` and `Quartz` it **would not work**. They do not run long and
  then succeed; they fail. `Integration` OOMs — a longer budget moves the
  failure later, it does not produce a pass. `Quartz` had not finished at 2400s,
  8x the budget it is alleged to be marginally over.
- For all three it **would hide the defect**. The same classes complete
  comfortably inside the existing 300s budget with the JIT disabled. Nothing
  about these classes is intrinsically too slow for the budget.

No `slowClasses` entries were added.

## Mechanism, and why it is already known

`[moving-young] fallback` means a young collection could not use the moving
(compacting) collector and fell back to the non-moving sweep. Two reasons appear
here — `unregistered-jit-frame-on-stack` (Flyway, Integration) and
`innermost-rbp-belongs-to-unguarded-callee` (Quartz) — both raised by the
conservative stack scan when it cannot prove a JIT frame is safe to move.

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
these three**: peaks of #4096 and #16384 are far past the 512 that filter was
built for. Whether the remaining fallbacks are further false positives or
genuine unregistered frames is not established here.

## The count is the triage signal

The fallback peak separates the two outcomes cleanly, and cheaply:

- **single digits → harmless.** Flyway hits 7 and still passes 73/73.
- **thousands → death spiral.** Integration #16384 → OOM; Quartz #4096 → no
  completion.

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

## 2026-08-10 reconciliation — confirmed on `Integration`/`Quartz`, but only the DEFAULT collector shows the named mechanism

Reconciling the 139-class non-passed union from the same-day `default`/`g1`/`zgc`
rerun (binaries `cratonvm-{default,g1,zgc}-20260808f.exe`, `dev@6365de194`, a later
tip than the `f695ca875` binary this page's own measurement used).
`IntegrationAutoConfigurationTests` and `QuartzEndpointWebIntegrationTests` are both
TIMEOUT/HANG at ~300s under **all three** collectors this round:

| Class | default | G1 | ZGC |
|---|---:|---:|---:|
| `IntegrationAutoConfigurationTests` | 300.104s | 300.200s | 300.170s |
| `QuartzEndpointWebIntegrationTests` | 300.123s | 300.012s | 300.109s |

The symptom (HANG at the 300s ceiling) is **collector-agnostic**. The *mechanism*
recorded above is only directly confirmed on the default collector this round,
though:

- **default**: both `.err.log`s are loud — `[moving-young] fallback #N` lines
  (Integration reached #256, Quartz #128, both climbing) interleaved with
  `gc::guard: young non-moving sweep was about to ZERO a span containing a LIVE
  (marked) object` retentions and `gen_heap: selective promotion: unwound N
  candidate(s)` warnings, active right up to the kill — the exact signature this
  page already names.
- **G1 and ZGC**: both `.err.log`s are near-silent — only the routine
  post-clinit-fixup/Mockito-self-attach boilerplate (7 lines each), zero GC/JIT
  diagnostic output for the whole run. `[moving-young]` is default-collector
  terminology (the non-moving *young* sweep fallback is specific to the
  Generational collector's compaction path), so its absence under G1/ZGC is
  expected and does not by itself mean those two collectors are clean — it means
  this page's specific instrumentation doesn't fire there.
- Both classes' `.out.log`s show steady progress on all three collectors (repeated
  Spring/Integration context start-stop or Quartz scheduler cycles, new timestamped
  output up to the moment of the kill on every collector) — no collector shows a
  dead/silent process, so none of these are the previously-fixed STW/AB-BA deadlock
  families.

**Not established by this rerun:** whether G1 and ZGC are hitting an equivalent
JIT-driven degradation via a different (un-instrumented) code path, or are simply
too slow under the JIT for an unrelated reason and would finish given more budget.
The standalone measurement above found the JIT itself is what breaks all three
classes on this page (`--nojit` clears every one, including on the default
collector) — that root explanation does not depend on which GC is compacting the
young generation, so the G1/ZGC HANGs are consistent with the same underlying
JIT-triggered cause without independently proving it. A `--nojit` A/B under G1 and
ZGC (the same protocol §"The measurement" used for default) would settle it.

Logs:
`apps/spring-boot-suite-runner/.suite/results/craton-nonpassed-{default,g1,zgc}-20260808f-s2/all-jit/logs/module_spring-boot-integration.org.springframework.boot.integration.autoco*-9d4bdd63bbf0.{out,err}.log`,
`.../module_spring-boot-quartz.org.springframework.boot.quartz.actuate.endpoint*-e3de43e38499.{out,err}.log`.

## Related

- `docs/known-issues/h2/h2-update-path-throughput-20260802.md` — names the
  non-moving fallback as a scaling target on an unrelated workload; same
  mechanism seen as throughput rather than as failure.
- `docs/internal/fixed-suite-bugs/springboot/flyway-integration-300s-margin-RETIRED-20260810.md`
- `docs/internal/fixed-suite-bugs/springboot/quartzendpointwebintegrationtests-recurring-timeout-RETIRED-20260810.md`

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-flyway` | `org.springframework.boot.flyway.autoconfigure.FlywayAutoConfigurationTests` |
| `module/spring-boot-integration` | `org.springframework.boot.integration.autoconfigure.IntegrationAutoConfigurationTests` |
| `module/spring-boot-quartz` | `org.springframework.boot.quartz.actuate.endpoint.QuartzEndpointWebIntegrationTests` |
