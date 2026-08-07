# `WebFluxAutoConfigurationTests` — recurring full-timeout HANG, borderline-slow class riding the 300s edge

**Status: OPEN — not root-caused; same "borderline-slow class tipped over
the timeout by concurrent load" shape as this batch's `QuartzEndpointWebIntegrationTests`
and `ZipContentTests` docs, not a hard deadlock as far as the evidence goes.**

## Not the same class as this batch's other WebFlux doc

`module/spring-boot-webflux` has two different classes in this triage batch:
`WebFluxManagementChildContextConfigurationIntegrationTests` (FAIL 5/5, a
harness `--add-opens` gap — see
`webfluxmanagementchildcontext-add-opens-not-forwarded-to-craton-20260807.md`)
and this one, `WebFluxAutoConfigurationTests` (HANG). Different class,
different symptom, unrelated root causes — noted explicitly because the
names are easy to conflate.

## Symptom

`org.springframework.boot.webflux.autoconfigure.WebFluxAutoConfigurationTests`
TIMED OUT at 300.036s on the 2026-08-06 Windows full-suite run
(`craton-fullsuite-windows-20260806-s4/all-jit/logs/module_spring-boot-webflux.org.springframework.boot.webflux.autoconfigure.W-b1ef3ecb60fd.{out,err}.log`).
The class reaches real Spring context setup — `.out.log` (2 lines) shows
`Hibernate Validator 9.1.0.Final` initializing and a `CglibAopProxy` warning
about `ResponseEntityExceptionHandler.handleException` being unproxyable —
before going silent. `.err.log` shows one `[moving-young] fallback #1:
reason=cross-thread-jit-peer` GC warning about 4 minutes into the run
(`02:57:23`, process started `02:53:13`), then one more clinit-fixup line at
`02:57:46`, then nothing until the kill at 300s. Unlike the Pulsar hang filed
alongside this batch, this class gets most of the way through its window
doing visible work first; unlike Quartz, the final ~3 minutes are silent
rather than continuing to log GC activity.

## Historical pattern: same class hangs intermittently, at the timeout boundary, across independent runs

| Run | Result | Seconds |
|---|---|---:|
| `craton-fullsuite-20260731` shard4 | **HANG** | 300.1 |
| `craton-rerun-20260731` | PASS | 172.7 |
| `craton-fullsuite-azure-20260802` | PASS | 163.7 |
| `craton-fullsuite-azure-20260805-s8` | PASS | 129.1 |
| `craton-fullsuite-windows-20260806-s4` (this run) | **HANG** | 300.0 |
| `craton-fullsuite-g1-20260807-s4` | PASS | 278.5 |
| `craton-fullsuite-zgc-20260807-s4` | **HANG** | 300.2 |

Clean PASSes range 129-278s — the 278.5s G1 PASS in particular is only ~22s
of margin under the 300s budget. This class is, like
`QuartzEndpointWebIntegrationTests` and `ZipContentTests` filed alongside it
in this same triage batch, inherently slow enough on CratonVM (no HotSpot
baseline timing was captured for this specific class this session, but 70
tests taking 130-280s is far from instant) that it sits right at the edge of
the fixed 300s per-class budget, and tips over into a full timeout
intermittently — three HANGs and four PASSes across seven independent runs,
with no obvious pattern by GC backend (both a HANG and a PASS occurred under
ZGC-adjacent and Generational conditions across this set, though the sample
size per backend is too small to draw a GC-specific conclusion).

## Root cause: not confirmed

Same open question as this batch's Quartz and ZipContentTests docs: whether
this is purely a throughput/margin problem (a class whose best-case runtime
already eats most of the 300s budget, pushed over by concurrent full-suite
host load — the same shape documented in detail for
`jooqautoconfigurationtests-timeout-regression-20260805.md`), or a genuine
intermittent stall specific to this class's context setup. The single
`cross-thread-jit-peer` GC fallback near the end of this run's window is a
normal, handled GC-quiescence fallback path (the young generation falling
back to a non-moving sweep because a live JIT frame on another thread
couldn't be proven to have a complete root map) and not inherently evidence
of a hang by itself — it appears in plenty of clean runs elsewhere in this
suite. No `--stack-dump-on-timeout` capture exists for any of the three HANG
rows above.

Whoever picks this up next should get a HotSpot baseline timing for this
specific class (none was found in this session's history search) to compute
a throughput ratio, and/or rerun it alone with `--stack-sample-ms` across the
last ~30s of a reproduced hang to see whether the main thread is making any
forward progress at all during the "silent" window.

## Affected classes

- `module/spring-boot-webflux` — `org.springframework.boot.webflux.autoconfigure.WebFluxAutoConfigurationTests`
