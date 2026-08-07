# `TomcatServletWebServerFactoryTests` / `JettyServletWebServerFactoryTests` HANG at 300s — steady forward progress, not a stall; the class needs ~2x the per-class budget

**Status: OPEN — throughput/budget gap, not a deadlock. Filed 2026-08-07.**

## Symptom

`craton-fullsuite-windows-20260806` (`-Xmx 2g`, 300s/class, Generational GC):

| Module | Class | Seconds |
|---|---|---:|
| `module/spring-boot-tomcat` | `org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactoryTests` | 300.170 (HANG) |
| `module/spring-boot-jetty` | `org.springframework.boot.jetty.servlet.JettyServletWebServerFactoryTests` | 300.070 (HANG) |

Logs:
`apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-windows-20260806-s4/all-jit/logs/module_spring-boot-tomcat.org.springframework.boot.tomcat.servlet.TomcatSer-3257b00b67ee.{out,err}.log`,
`apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-windows-20260806-s3/all-jit/logs/module_spring-boot-jetty.org.springframework.boot.jetty.servlet.JettyServle-2b982ecd0797.{out,err}.log`.

## Not the previously-fixed hangs — checked against both existing docs

Both classes have substantial fix history:
`docs/internal/fixed-suite-bugs/springboot/tomcatservletwebserverfactorytests-stw-takeover-hang-FIXED.md`
(an STW cross-thread JIT-takeover deadlock, fixed 2026-07-27, re-verified 2026-08-06 with 72
clean reruns) and
`docs/internal/fixed-suite-bugs/springboot/jetty-webserver-factory-poststartup-timeout-and-reflective-supertype-residuals-FIXED.md`
(several distinct hangs/crashes fixed through 2026-07-21, ending in an AB-BA
`class_manager`/`vtable_manager` lock-ordering deadlock, "Final resolution," verified
113/113 with 0 aborted on JIT and `--nojit`).

Neither signature matches this run:

- **No STW stall.** The STW bug's signature is `WARN ... STW cross-thread JIT takeover is
  still waiting for cooperative mutators ... pending=1 taken=0`, repeating forever with
  *zero* other log output once it hits. That line does not appear anywhere in either
  `.err.log` here. The `.out.log` is still advancing (new `INFO`/`WARN` lines with new
  timestamps) in the seconds immediately before the kill in both cases — the opposite of the
  documented "output simply stops" symptom.
- **No AB-BA deadlock / dead thread count.** That bug's signature (caught live via `cdb`) is
  exactly 5 threads, `main-vm` parked in `LockSupport.park()` and `Common-Cleaner` blocked
  acquiring `class_manager`'s read lock — a snapshot this investigation did not need to take,
  because the process was never actually idle: both logs show continuous, monotonically
  advancing timestamped output (new Tomcat/Jetty connector start/stop cycles) right up to the
  300s cutoff.

## What the logs actually show: steady progress, budget simply runs out

**Tomcat** (`TomcatSer-3257b00b67ee.out.log`, 581 lines): process starts ~02:33:14
(`.err.log` first `Post-clinit fixup` line), reaches its 62nd `Starting Servlet engine`
cycle at `02:38:13`, then the process is killed — no repeated/stuck timestamp, each cycle
(`Tomcat initialized` → `Http11NioProtocol Initializing` → `Starting Servlet engine` →
`Started`/error → `Stopping`) takes on the order of 4-5s and the next one starts
immediately after. `AbstractServletWebServerFactoryTests` (the shared Tomcat/Jetty base,
`module/spring-boot-web-server/src/testFixtures/.../servlet/AbstractServletWebServerFactoryTests.java`)
contributes 84 `@Test` methods, plus 42 of its own — **126 total** — most of which start and
stop a fresh embedded container.

**Jetty** (`JettyServle-2b982ecd0797.out.log`, 1163 lines): process starts ~03:13:20, reaches
its 57th `Jetty started` line at `03:18:16`, killed shortly after — same shape, ~5s/cycle,
continuously advancing. Jetty's own class contributes 27 `@Test` methods plus the same 84
from the shared base — **111 total**.

Extrapolating each class's own observed per-cycle rate to its full test count lands well past
300s (Tomcat: 299s/62 cycles × 126 ≈ 607s; Jetty: 296s/57 cycles × 111 ≈ 576s) — both roughly
**2x** the 300s budget, consistent with the previous fix doc's own historical timing for the
same classes ("60 server-start cycles in 571s" for Tomcat/Jetty-shaped runs; "17 cycles, 3-12s
each, no large spikes" post-fix for Jetty specifically). Neither log shows a single outlier
cycle (no 90-200s spike like the pre-fix Xerces/TLD-scan residual the Jetty doc also
documents) — the overrun here is uniform per-cycle cost accumulated over many cycles, not one
stuck operation.

For scale: the HotSpot baseline (`hotspot-baseline-20260717`) completes
`TomcatServletWebServerFactoryTests` (129 tests, 3 skipped) in **137.6s** and
`JettyServletWebServerFactoryTests` (111 tests, 4 skipped) in **141.1s** — both comfortably
inside 300s. CratonVM's ~4-5s/cycle vs. HotSpot's ~1-1.3s/test puts CratonVM at roughly
**4-4.5x HotSpot's wall time** for this workload (repeated embedded-container
start/stop, TLS/keystore setup, Mockito self-attach), the same general shape (if less
extreme) as the previously-filed
`docs/internal/springboot/CRATONVM_BUGS`-adjacent "N x HotSpot, budget runs out before a
large multi-cycle class finishes" pattern seen elsewhere in this suite (e.g.
`JooqAutoConfigurationTests`, `FlywayAutoConfigurationTests`).

## `.err.log` noise, checked and ruled out as the cause

Both `.err.log`s show the usual per-class boilerplate — `[moving-young] fallback #N` GC
warnings (non-moving sweep fallback; a known, separate throughput concern, not a stall — see
`docs/known-issues/reference_non_moving_young_sweep_degenerates_into_an_unusable_free_list.md`-style
notes), repeated `WARN keystore: JKS key integrity check failed (wrong password?)` (present in
several other classes' logs too, e.g. the earlier `crashfail-20260717-crash-cluster-FIXED.md`
find, and not correlated with any abort here), and the one-time Mockito self-attach notice.
None of these repeat in a tight loop or correlate with the moment of the kill; they are spread
evenly across the whole run, consistent with "normal per-cycle cost," not a fault.

## Root cause — not identified in this pass

No single hot method was profiled in this pass (would need `--stack-sample-ms` or a `cdb`
live-attach mid-run, per the technique the Jetty fix doc used for its own residuals). Worth
checking, in order of plausibility given this suite's other findings:

1. Whether this is (still, post-08-06 `class_manager`/vtable fixes) the same general
   "Class.getDeclaredMethods() / reflection-heavy Spring bean introspection is O(n) or worse
   per call" pattern root-caused for jOOQ
   (`docs/known-issues/springboot/jooqautoconfigurationtests-timeout-regression-20260805.md`)
   and flagged there as "likely the load-bearing term in the Tomcat annotation-scan wall
   (224-259x) and the webapp-deploy wall (234x) already on file" — both Tomcat and Jetty
   factory tests build a fresh `ApplicationContext`-adjacent bean graph on every cycle.
2. Whether the per-cycle keystore/TLS setup (`JKS key integrity check failed` fires twice per
   cycle in both logs) carries meaningful fixed cost across ~60-130 repetitions.
3. A direct `--stack-sample-ms 200` capture of either class in isolation (via
   `apps/spring-boot/sb-runner`, one class at a time, no 300s ceiling) would settle this in one
   run without needing the full suite harness.

## Affected classes

- `module/spring-boot-tomcat` — `org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactoryTests` (HANG, 300.170s, ~62/126 cycles reached)
- `module/spring-boot-jetty` — `org.springframework.boot.jetty.servlet.JettyServletWebServerFactoryTests` (HANG, 300.070s, ~57/111 cycles reached)
