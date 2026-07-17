# `spring-boot-webmvc` residuals: root-path forward-to-error-page returns 404 instead of invoking the handler; two classes with many per-test full-context boots time out at 300s

**Status: OPEN — found 2026-07-17**

## Cluster A — `RemappedErrorViewIntegrationTests#forwardToErrorPage`: root mapping never invoked under a context path, 404 instead of the expected 500-forward

### Symptom

`RemappedErrorViewIntegrationTests` has 2 tests; 1 fails:

```
JUnit Jupiter:RemappedErrorViewIntegrationTests:forwardToErrorPage()
    => java.lang.AssertionError:
Expecting actual:
  "{"timestamp":"2026-07-17T21:28:09.340Z","status":404,"error":"Not Found","path":"/spring/"}"
to contain:
  "500"
       org.springframework.boot.webmvc.autoconfigure.error.RemappedErrorViewIntegrationTests.forwardToErrorPage(RemappedErrorViewIntegrationTests.java:67)
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-webmvc.org.springframework.boot.webmvc.autoconfigure.error.RemappedErrorVie-a4609b4dd81e.out.log`

The test's fixture (`RemappedErrorViewIntegrationTests.java:70-84`) registers
a controller with `@RequestMapping("/")` whose `home()` method unconditionally
`throw new RuntimeException("Planned!")`, plus a custom error page registered
at `/spring/error` via `ErrorPageRegistrar`. The passing sibling test,
`directAccessToErrorPage`, requests `/spring/error` directly and gets the
expected content — so the context-path (`/spring`) wiring and the
`/spring/error` mapping both work. `forwardToErrorPage` requests `/spring/`
(the root path under that context path) expecting `home()` to run, throw,
and have the container forward to the registered error page (producing a
500-status body) — but instead gets an **immediate 404 "Not Found"** for
`/spring/`, meaning `home()` is never invoked at all (a real
`RuntimeException` reaching Tomcat's error-page forwarding would produce a
500-status body, not a 404 — a 404 means Spring MVC/Tomcat never found a
handler mapping for `/spring/` in the first place).

### Root cause

**Not confirmed — hypothesis only, no CratonVM source consulted for this
one (nothing in the trace points at a specific native function; this looks
like a servlet-mapping/path-matching correctness gap, not a crash/exception
with a stack trace to follow).** The strongest hypothesis is that
CratonVM's Tomcat/DispatcherServlet integration does not correctly match
the *exact-root* URL pattern (`@RequestMapping("/")`, which Spring's
`PathPatternParser` treats specially — it matches both `""` and `"/"` under
the servlet's mapping) when a non-empty context path is also configured,
so the request never reaches `DispatcherServlet` and Tomcat's own
404-not-found default response short-circuits before any
Spring/application code (including the exception-throwing handler) runs.
This is a guess based on the symptom shape (immediate 404, not a 500 with
wrong content) — needs a live repro (a minimal Spring Boot app with a
context path and a root `@RequestMapping`, hitting it directly) to confirm
or refute; not attempted this session.

### Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-webmvc` | `org.springframework.boot.webmvc.autoconfigure.error.RemappedErrorViewIntegrationTests` (1 of 2 tests) |

## Cluster B — `WebMvcAutoConfigurationTests` / `BasicErrorControllerIntegrationTests` HANG: 300s timeout during many per-test-method full-context/embedded-Tomcat boots

### Symptom

Both classes HANG at exactly the suite's 300s timeout (`results.tsv`:
`WebMvcAutoConfigurationTests` 300.061s, `BasicErrorControllerIntegrationTests`
300.191s — both `TIMEOUT`/`HANG`, not a silent process death). Unlike the
[`modifiedclasspath-aether-network-hang-cluster.md`](modifiedclasspath-aether-network-hang-cluster.md)
and
[`graphql-security-autoconfiguration-early-hang.md`](graphql-security-autoconfiguration-early-hang.md)
HANG shapes, both classes show substantial, continuing real application
activity throughout their `.err.log`s — not silence, not a single benign
warning on repeat:

- `WebMvcAutoConfigurationTests` — repeated `Hibernate Validator`
  `ResourceBundleMessageInterpolator`/`ResourceLoaderHelper` DEBUG cycles
  (41 in the observed window), consistent with each of the class's many
  `ApplicationContextRunner`-based test methods independently bootstrapping
  bean validation.
- `BasicErrorControllerIntegrationTests` — 20 complete embedded-Tomcat
  boot/teardown cycles (`Initializing ProtocolHandler ["http-nio-auto-N"]`
  ... `Destroying Spring FrameworkServlet 'dispatcherServlet'`, `N` counting
  up to 20) visible in `.err.log` before the process is killed. The class
  has **83 `@Test` methods**
  (`apps/spring-boot/module/spring-boot-webmvc/src/test/java/.../BasicErrorControllerIntegrationTests.java`),
  each independently booting a full embedded Tomcat + `DispatcherServlet`
  context — at the observed pace (20 cycles in ~68-136s of visible activity
  after an initial ~160-200s of pre-log VM/classloading silence — see below),
  completing all 83 within 300s total is arithmetically implausible even at
  a steady, non-stuck pace.

Timestamp reconstruction (from `results.tsv` wall time minus the last
timestamped log line) shows, for both classes, the **last log line lands
within roughly 0-1s of the exact 300s kill point** — i.e. the process was
still actively producing log output right up until the watchdog fired, not
sitting silent for a long tail beforehand. This is a materially different
signature from the two other HANG-cluster docs above (which show a long
silent/near-silent stretch before the kill) and points at **cumulative
per-boot overhead exceeding the timeout budget**, not a deadlock partway
through.

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-webmvc.org.springframework.boot.webmvc.autoconfigure.WebMvcAutoConfigurationTests.err.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-webmvc.org.springframework.boot.webmvc.autoconfigure.error.BasicErrorContro-957a1d0f4289.{out,err}.log`

### Root cause

**Not confirmed as a specific logic bug — most likely a throughput/overhead
problem** (CratonVM taking substantially longer than HotSpot per
context-boot cycle), same general shape as the still-open interpreter-
throughput residual in
`docs/known-issues/tomcat-08-07/silent-hang-no-signature-cluster.md`
(different suite/classes, but the same "genuinely busy the whole time, just
too slow to fit the timeout" conclusion, confirmed there via live CPU-time
sampling — not repeated here). Not proven — an alternative explanation
(progress *appears* continuous because of log-buffering/flush timing near
the kill, while the process actually stalled significantly earlier and only
flushed a backlog right before being killed) has not been ruled out without
a live debugger attach.

**`BasicErrorControllerIntegrationTests` discrepancy check (per this
session's triage instructions):** this exact class has a **prior, `FIXED`**
doc —
[`../../internal/springboot/basiccontroller-stale-pointer-invokevirtual-aqs-conditionnode-crash-FIXED.md`](../../internal/springboot/basiccontroller-stale-pointer-invokevirtual-aqs-conditionnode-crash-FIXED.md) —
for a **different** symptom: a fatal `ClassCastException`/process-CRASH
caused by a young-GC `young_object_starts` forwarding-walk truncation bug
(`gc/src/gen_heap.rs`). That doc's own text already anticipates today's
finding: *"Re-ran `BasicErrorControllerIntegrationTests` against a fresh
post-merge build: the fatal `ClassCastException`/process-CRASH is gone (now
`HANG` at the 300s timeout — a different, unrelated outcome not investigated
further here)."* This is exactly the HANG documented here. **Verified the
GC fix is genuinely present and complete in this worktree**, not just
partially merged as that doc's own "Related" section worried it might be:
`grep -n skip_free_blocks gc/src/gen_heap.rs` shows the `young_object_starts`
walk (`gc/src/gen_heap.rs:3776-3793`, comment header explicitly labeled
`cce0079 ROOT FIX (2026-07-16)`) now calls `skip_free_blocks` against a
merged free-list+TLAB-tail skip set, matching the sibling `young_object_ranges`
walk — i.e. this worktree has the **full** fix, not just the partial
GAP-filler-only one that doc flagged as insufficient. So: the CRASH is
confirmed fixed; the residual HANG is a **separate, not-yet-investigated**
issue (consistent with, not contradicting, the FIXED doc's own scope), which
this doc now tracks as an open throughput/timeout question rather than a
memory-safety bug.

### Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-webmvc` | `org.springframework.boot.webmvc.autoconfigure.WebMvcAutoConfigurationTests` |
| `module/spring-boot-webmvc` | `org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests` |
