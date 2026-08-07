# `QuartzEndpointWebIntegrationTests` — recurring full-timeout HANG, distinct from the CLOSED 2026-08-03 NPE/Jersey doc

**Status: OPEN — not root-caused; likely a borderline-slow class tipped over the 300s budget under full-suite concurrent load, but a genuine stall is not ruled out.**

## Not the already-closed doc

`docs/internal/fixed-suite-bugs/springboot/quartzendpoint-webflux-sortedset-first-npe-and-jersey-hk2-perlookup-FIXED-20260803.md`
closed this exact class 2026-08-03, verifying 7/7 JIT runs and 2/2 `--nojit`
runs all clean (45/45 each) after two prior symptom families
(`SortedSet.first()` NPE, Jersey HK2 `PerLookup` resolution) both turned out
to be side effects of unrelated fixes landed earlier that week. That
verification's signature was a normal JUnit summary with 0 failures — this
run's signature is a full-timeout HANG with **zero JUnit output at all**, so
before treating this as a regression of that closed doc the two were checked
against each other explicitly: they don't match, and this is filed as its own
open issue rather than reopening that one.

## Symptom

`module/spring-boot-quartz`'s
`org.springframework.boot.quartz.actuate.endpoint.QuartzEndpointWebIntegrationTests`
TIMED OUT at 300.132s on the 2026-08-06 Windows full-suite run
(`craton-fullsuite-windows-20260806-s4/all-jit/logs/module_spring-boot-quartz.org.springframework.boot.quartz.actuate.endpoint.-e3de43e38499.{out,err}.log`).
Unlike the Pulsar hang filed alongside this one (see
`pulsarautoconfigurationtests-onbeancondition-multivaluemap-classcastexception-flake-20260806.md`'s
2026-08-07 update), this class makes real, substantial progress before
stalling: the `.out.log` has 309 lines, showing the test class repeatedly
standing up and tearing down full Tomcat/Jersey/Netty web server stacks — at
least 18 separate "Tomcat started" cycles plus interleaved Reactor Netty
starts, each with its own Jersey `ApplicationHandler`/Spring
`DispatcherServlet` initialization. The last line in the log is a clean
`Netty started on port 55853 (http)` at `01:14:14.110`; the process was
started around `01:09:12` (300.132s budget), so it stalled in the final
seconds of its window rather than early. The `.err.log` shows the same
`net/bytebuddy/implementation/bind/annotation/Argument$Binder.bind` JIT
compile-bail seen in the Pulsar doc (identical `code_len=244 capacity=62336
wanted=68331`) near the start, but — unlike Pulsar — GC activity
(`gc_quiescence`/`gen_heap` warnings) continues right up to `01:12:30`, close
to the very end of the window, so this class is not stalling silently the
way Pulsar does; it is running out of budget while still doing real work.

## Historical pattern: recurring intermittent full-timeout HANG, alternating with PASS/FAIL, across many independent runs

| Run | Result | Seconds |
|---|---|---:|
| `hotspot-baseline-20260717` shard5 | PASS | 121.3 |
| `craton-rerun-20260717` shard5 | FAIL (real failure, `ApplicationContextException`) | 221.8 |
| `craton-rerun-20260723` shard3 | **HANG** | 300.1 |
| `craton-rerun-20260728` shard1 | PASS | 645.3 (longer budget) |
| `craton-fullsuite-20260731` shard4 | **HANG** | 300.0 |
| `craton-rerun-20260731` | **HANG** | 300.1 |
| `craton-hangverify-20260731` | FAIL, 4/45 (not a hang) | 738.7 (longer budget) |
| `craton-fullsuite-azure-20260802` | FAIL, 3/45 | 295.4 |
| `craton-fullsuite-azure-20260805-s7` | PASS | 244.2 |
| `craton-residual32-20260804-s3` | PASS | 166.4 |
| `craton-rerun-20260801` | FAIL, 3/45 | 381.7 (longer budget) |
| `craton-fullsuite-windows-20260806-s4` (this run) | **HANG** | 300.1 |
| `craton-fullsuite-g1-20260807-s4` | **HANG** | 300.1 |
| `craton-fullsuite-zgc-20260807-s4` | **HANG** | 300.2 |

Clean PASSes span 121-645s — this is an inherently heavy class (spins up many
full web server stacks) that is already slow even when it works, and with the
default 300s budget it sits right on the edge: several genuine PASSes take
longer than 300s under a more generous timeout (645s, 738s-with-failures),
meaning the 300s default has no real margin for this class even in the
best case. The HotSpot baseline (121s, one data point) suggests CratonVM's
per-cycle cost for the repeated Tomcat/Jersey/Netty start-stop churn this
class does is itself the throughput problem, in the same shape as the
already-documented `JooqAutoConfigurationTests` timeout
(`jooqautoconfigurationtests-timeout-regression-20260805.md`) — a class whose
per-test-body cost is 90-175x HotSpot's, tipping an otherwise-passing class
over a fixed timeout. Not measured directly for this class this session
(would need a HotSpot re-baseline and a longer-budget CratonVM run to
compute a ratio the way that doc did for jOOQ).

The three most recent runs (windows/g1/zgc, all 08-06/08-07, all HANG) were
all part of the same concurrent 4-shard full-suite sweep referenced elsewhere
in this triage batch — consistent with genuine per-class throughput cost
plus concurrent host load from sibling shards pushing an already-marginal
class over the timeout, rather than a new deterministic defect. Not proven:
a real deadlock specific to the repeated web-server start/stop cycle (e.g. a
Netty `EventLoopGroup` shutdown that occasionally never completes) would
produce the same observed shape and cannot be ruled out from these logs
alone.

## Root cause: not confirmed

Two candidate mechanisms, neither confirmed or eliminated this session:

1. **Throughput, not a hang** — same shape as the jOOQ timeout doc: a
   per-test-body cost that is already close to or over 300s on a clean run,
   made worse by concurrent full-suite host load. The 645s/738s
   longer-budget PASSes/FAILs above support this — the class needs
   meaningfully more than 300s even to finish (successfully or not) in the
   best case seen.
2. **A genuine intermittent stall** in the repeated Tomcat-stop/Netty-start
   server lifecycle churn this class does more than most (at least 18 full
   server start/stop cycles observed in one run) — possibly related to the
   same `net/bytebuddy/.../Argument$Binder.bind` JIT-bail neighborhood the
   Pulsar hang (filed alongside this one) also hits, though this class's GC
   activity continuing until near the very end argues against a hard,
   early stall of the kind Pulsar shows.

No `--stack-dump-on-timeout` capture exists for any of the HANG rows above
(the suite runner disables the watchdog by default in favor of its own
per-class timeout). Whoever picks this up next should get a real HotSpot
baseline timing and a longer-budget CratonVM timing to compute a throughput
ratio the way `jooqautoconfigurationtests-timeout-regression-20260805.md`
did, before assuming this is a hang at all.

## Affected classes

- `module/spring-boot-quartz` — `org.springframework.boot.quartz.actuate.endpoint.QuartzEndpointWebIntegrationTests`
