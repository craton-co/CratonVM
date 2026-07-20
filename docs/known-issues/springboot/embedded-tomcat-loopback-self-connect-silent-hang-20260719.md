# Embedded-Tomcat integration tests now hang indefinitely (not timeout) on the self-connect back to their own just-started server

**Status: OPEN — found 2026-07-19, current `dev` tip (`e1e5d99ee`)**

## Symptom

`RemappedErrorViewIntegrationTests` (and `BasicErrorControllerIntegrationTests`,
`WebMvcAutoConfigurationTests` — see below) start a real embedded Tomcat via
`@SpringBootTest(webEnvironment = RANDOM_PORT)`, then issue an HTTP request
back to `localhost:<ephemeral port>` from the same process via
`TestRestTemplate`/`RestTemplate`. On current `dev`, this now hangs
**indefinitely with zero further log output** after
`WebApplicationContextInitializer -- Root WebApplicationContext:
initialization completed` — no exception, no timeout, no further JUnit
progress. Reproduced 3× in a row against the same freshly-built release
binary:

- `-Parallel 2`, both target classes together: both `HANG` at their
  timeout (900s / 1800s).
- `-Parallel 1` (serial), same two classes: both `HANG` again at 1800s
  each (60 min wall total).
- `RemappedErrorViewIntegrationTests` **alone**, `-Parallel 1`,
  `-TimeoutSec 180`: still `HANG` at 180s, confirming this is not a
  parallel-run self-contention artifact.

For comparison: the exact same test class, same fix, same build recipe,
against a build from ~4 hours earlier **without** the intervening ~91
`origin/dev` commits, passed in 25.4s (`RemappedErrorViewIntegrationTests`
2/2) and 528.5s (`BasicErrorControllerIntegrationTests` 26/26 — see
[`../../internal/springboot/webmvc-error-forward-and-multiboot-timeout-cluster-FIXED.md`](../../internal/springboot/webmvc-error-forward-and-multiboot-timeout-cluster-FIXED.md)).
Merging current `dev` into that same branch and rebuilding (no other change)
reproduces the hang consistently. This strongly implicates something in the
intervening `dev` commits, not the fix being validated in that doc.

## Why this is likely NOT the two fixes just merged in the same commit

The two fixes landed alongside this finding
(`docs/internal/springboot/webmvc-error-forward-and-multiboot-timeout-cluster-FIXED.md`)
are:
- `mapping_match_static` (`native-builtins/src/lib.rs`): only affects
  request-path -> handler mapping *after* a request has already arrived at
  `DispatcherServlet`. Cannot affect whether a TCP connect/HTTP request ever
  gets sent or answered.
- `http_parse_url` (`native-builtins/src/net_phase_e.rs`): only changes
  behavior for URLs where a `?`/`#` immediately follows the authority with
  no `/`. A normal `http://localhost:<port>/spring/error`-shaped request
  (what these tests send) takes the exact same code path as before the
  change.

Neither plausibly explains a hang that occurs before any request-level log
line is even reached. The hang is reproducible on this exact commit,
but the mechanism looks environmental/host-level or a pre-existing dev-side
accept-thread/readiness race, not something introduced by either fix.

## Relationship to the existing `webclient-loopback-self-connect-timeout-os10060-cluster.md`

That doc (found 2026-07-17, still OPEN, root cause not confirmed) describes
the *same shape* of problem — a test self-connecting to its own just-started
embedded server over loopback — but with a **different, milder** symptom: a
fast, real OS-level `os error 10060` (`WSAETIMEDOUT`) failure, not a
multi-minute-plus silent hang with zero progress and no eventual timeout at
all. That doc's own hypothesis 2 (a CratonVM accept-thread readiness race —
`WebServer.start()` returning before the listener's `accept()` loop is
actually servicing connections) would explain a fast timeout; it does not
obviously explain total silence for 30+ minutes with no OS-level error ever
surfacing. This may be the same underlying race made worse by some
intervening change, or a second, distinct bug in the same problem family.
Filed as a separate doc rather than folded into that one because the
symptom (hang vs. fast-fail) and severity (blocks completion entirely vs.
one test method failing) are different enough to need independent
confirmation.

## What would confirm this

- `git bisect` (or a manual midpoint build) across the ~91 `dev` commits
  pulled into `codex/fix-webmvc-error-timeout-20260718-019f768e` between its
  fork point and `dev`'s tip at merge time (`8783860ec`), rerunning
  `RemappedErrorViewIntegrationTests` alone (fast, 25s expected) at each
  candidate.
- A live thread/stack sample (or `CRATONVM_SYMBOLIZE=1` + a debugger attach)
  of the hung process to see whether the main test thread is blocked in a
  socket connect/read call, or something else entirely (e.g. class-loading,
  JIT compilation stall).
- Retry on a quieter host — this box was observed with 18-19 concurrent
  `claude.exe` processes and (in an earlier, unrelated build on the same
  session) had dropped to ~290MB free physical RAM out of 66.8GB, so genuine
  host-level scheduling starvation has not been fully ruled out even though
  the isolated single-class 180s run argues against it being the sole cause.

## Affected classes (confirmed)

| Module | Class |
|---|---|
| `module/spring-boot-webmvc` | `org.springframework.boot.webmvc.autoconfigure.error.RemappedErrorViewIntegrationTests` |
| `module/spring-boot-webmvc` | `org.springframework.boot.webmvc.autoconfigure.error.BasicErrorControllerIntegrationTests` |

Likely affects any class in the same self-connect-over-loopback shape
(see the class list in `webclient-loopback-self-connect-timeout-os10060-cluster.md`),
not confirmed this session.
