# `TomcatServletWebServerFactoryTests` / `JettyServletWebServerFactoryTests` HANG at 300s — steady forward progress, not a stall; the class needs ~2x the per-class budget

**Status: OPEN for the throughput gap only. The 82-of-90 `port 8080` failures
described below are FIXED (2026-08-10): the `HashMap` miss was an identity hash
that changed between `put` and `get`, because the key was first hashed inside
its own `synchronized` block. The diagnosis page is retired to
`internal/fixed-suite-bugs/springboot/hashmap-get-misses-a-key-its-own-entryset-yields-FIXED-20260810.md`,
which carries the trigger and the fix. Filed 2026-08-07.**

A twelve-class embedded-server sample re-run after the fix reports **zero**
`listen on port 8080` occurrences, where every such class reported them before.
`TomcatServletWebServerFactoryTests` and `JettyServletWebServerFactoryTests`
still exceed 300s, so what remains here is the throughput gap the rest of this
page describes — and it is now the whole of what this page is about.

## 2026-08-10 update — the Tomcat half was misdiagnosed

Run to natural completion (no 300s ceiling), `TomcatServletWebServerFactoryTests`
**finishes in 813.7s with 132 tests — and 90 of them fail.** The class does not
hang, and giving it more budget does not make it pass. HotSpot on the same host:
129 tests, **0 failures**, 94.9s.

82 of those 90 failures are
`ConnectorStartFailedException: Connector configured to listen on port 8080
failed to start` — on a fixture whose `getFactory()` is
`new TomcatServletWebServerFactory(0)`, i.e. "pick an ephemeral port". CratonVM
starts a connector on **8080** anyway, 99 times over; HotSpot's log never
mentions 8080 at all.

Root cause, reproduced standalone in ~10 seconds: Spring parks the connectors it
temporarily removed in a `Map<Service, Connector[]>` and restores them on
`start()`. On CratonVM that `get` **misses a key the same map's `entrySet()`
yields** — one-entry map, `==`-identical key, equal `hashCode`. The connectors
are never restored, and `Tomcat.getConnector()` then *fabricates* one on port
8080, which collides and fails. Full evidence, and five refuted alternative
mechanisms, in the linked doc.

So the "~4-4.5x HotSpot, budget runs out" framing below describes a real
throughput gap that is still open, but it is not why these tests fail. The
extrapolation under "What the logs actually show" assumed every cycle was
productive work; most of the Tomcat cycles were failing connector starts.

**The Jetty half is not settled either way.** A 2400s run with
`--stack-sample-ms=200` produced `tests=0` — it never reported a single test,
having spent ~20 minutes inside one `ContextHandler`/TLD-scan start
(`04:27:30` → `04:47:28` for a single context) and reaching only 19 server
cycles. That is the opposite of the "no single outlier cycle" claim below, but
the sampler's own overhead is uncontrolled in that run and the host was carrying
14 concurrent CratonVM processes from 6 worktrees, so it does not stand as a
measurement. Jetty needs an unsampled run on an idle host before anything is
concluded about it.

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
`fixed-suite-bugs/springboot/tomcatservletwebserverfactorytests-stw-takeover-hang-FIXED.md`
(an STW cross-thread JIT-takeover deadlock, fixed 2026-07-27, re-verified 2026-08-06 with 72
clean reruns) and
`fixed-suite-bugs/springboot/jetty-webserver-factory-poststartup-timeout-and-reflective-supertype-residuals-FIXED.md`
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
`springboot/CRATONVM_BUGS`-adjacent "N x HotSpot, budget runs out before a
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

## 2026-08-10 reconciliation — `TomcatServletWebServerFactoryTests` confirmed collector-agnostic, still HANG on G1 (not FAIL)

Reconciling the 139-class non-passed union from the same-day `default`/`g1`/`zgc`
suite rerun (binaries `cratonvm-{default,g1,zgc}-20260808f.exe`, `dev@6365de194`).
`TomcatServletWebServerFactoryTests` is TIMEOUT/HANG at ~300s under **all three**
collectors this round — default 300.135s, G1 300.106s, ZGC 300.200s, per
`results.tsv`'s `status`/`note` columns in each of
`craton-nonpassed-{default,g1,zgc}-20260808f-s2/all-jit/results.tsv`. (An
upstream note for this reconciliation batch described the G1 row as FAIL rather
than HANG; that does not match what this rerun's own `results.tsv` rows record —
worth double-checking against whatever produced that note, but the raw data
checked here is unambiguously HANG on all three.)

All three logs show exactly this page's already-established signature — steady,
uninterrupted forward progress, not a stall:

| Collector | `.out.log` lines | `Starting Servlet engine` cycles reached | port-8080 fabrication lines |
|---|---:|---:|---:|
| default | 619 | 65 | 0 |
| G1 | 496 | 52 | 0 |
| ZGC | 571 | 60 | 0 |

Each collector reaches roughly half the class's ~126 total cycles by the 300s
cutoff (consistent with this page's own ~2x-budget extrapolation), with new
timestamped Tomcat lifecycle output right up to the kill in every case — none of
the three shows a stuck/repeating timestamp or the STW-stall/AB-BA-deadlock
signatures this page already ruled out. **Zero `8080` occurrences in any of the
three `.out.log`s** confirms the `Tomcat.getConnector()`-fabricates-8080 bug (now
fixed, see the retirement note at the top of this page and
`fixed-suite-bugs/springboot/hashmap-get-misses-a-key-its-own-entryset-yields-FIXED-20260810.md`)
has not regressed on any collector — what remains is purely the throughput gap
this page already characterizes as open.

**Collector-agnostic (reproduces under Generational, G1, and ZGC)** — same
symptom, same rough cycle-count/budget ratio, on all three. No root cause beyond
what this page already documents (§"Root cause — not identified in this pass");
this reconciliation adds G1/ZGC coverage to what was previously a default-collector-only
measurement (the original 2026-08-06/2026-08-10 runs referenced above did not
include a G1 or ZGC arm for this class), it does not change the open question.

Logs:
`apps/spring-boot-suite-runner/.suite/results/craton-nonpassed-{default,g1,zgc}-20260808f-s2/all-jit/logs/module_spring-boot-tomcat.org.springframework.boot.tomcat.servlet.TomcatSe*-3257b00b67ee.{out,err}.log`.

## Affected classes

- `module/spring-boot-tomcat` — `org.springframework.boot.tomcat.servlet.TomcatServletWebServerFactoryTests` (HANG, 300.170s, ~62/126 cycles reached; reconfirmed 2026-08-10 as HANG at ~52-65/126 cycles on default, G1, and ZGC alike)
- `module/spring-boot-jetty` — `org.springframework.boot.jetty.servlet.JettyServletWebServerFactoryTests` (HANG, 300.070s, ~57/111 cycles reached)
