# L7 — Phase 4: the definition of done itself

> **RETIRED 2026-08-28 — the lane is closed and this brief has nothing open
> left in it.** The result, the numbers, the host and the exact commands are in
> `known-issues/jdk-only/the-definition-of-done-run-on-the-three-real-workloads-20260828.md`,
> which stays in `known-issues/` because it ends with residuals somebody still
> has to pick up.
>
> **What this brief got wrong, and it is the useful part.** §2 says the blocker
> is that none of the three workloads is checked out. The honest form was *"not
> on the Windows box"*: all three are checked out and built on `azure-host-2`,
> which §2 itself names as the place to look first. `apps/h2database/h2`
> (1114 + 688 compiled classes), `apps/tomcat` (2801 test classes and a full
> `output/build` CATALINA_BASE) and `apps/spring-boot/smoke-test` (108 modules,
> each with a resolved `build/cratonvm-test-cp.txt`). A blocker written from one
> host reads as a property of the project.
>
> **What it got right and what it cost to re-learn.** §3's three instrument
> traps are all still live, and a fourth joined them: `--Xmx` takes its size as
> a SEPARATE argument, so `--Xmx2g` is rejected during argument parsing with
> exit 2 and no report — indistinguishable from the `System.exit` case if you
> only check for the file. §4's insistence that a fabrication *request* is not a
> failure is the rule that made the screen usable.
>
> **The outcome.** Five arms over the three workloads, all running to
> completion under `--jdk-only`, `compatibility_classes: 0` and
> `synthetic_stub_invocations: 0` on every one, and every
> `compatibility-class-requested` row named with its requester `file:line`.
> Getting there took four VM fixes, and **not one of them was a `--jdk-only`
> defect**: all four were mode-independent, and strict mode is simply where
> they became reachable, because refusing a synthetic stub puts the real JDK
> bytecode in front of a native that had never been called.

**Read `HANDOFF-20260828-SCOPE.md` first.**

**Owner: unclaimed.** L5 (`claude/jdk-only-mode-handoff-09b48c`, worktree
`h2-known-issues-206dee`) is the only lane currently running.

**This lane is not Phase 2.** It has no row count and it is the only lane that
can actually declare the goal met. It is also the only one that may be BLOCKED
rather than merely large — read §2 before committing to it.

## 1. What you are proving

`docs/feature-designs/jdk-only-completion-roadmap.md` §6:

> A Spring Boot application, a servlet container serving HTTPS, and a JDBC
> workload each run to completion under `--jdk-only` with **no fabricated class
> instantiated, whatever its package** — screened against the refused-class set
> the VM reports, not against a prefix.

The prefix clause is load-bearing: **six of the nine fabricated classes the
roadmap names do not match `cratonvm/internal/`**, so a prefix screen would have
reported Phase 1 clean with every one of them still fabricated. Screen on the
report's own rows.

## 2. The blocker to resolve first

**None of the three workloads is checked out on this host.** Only their runners
are:

```text
apps/spring-boot/sb-runner
apps/h2database-suite-runner/     (expects $H2_ROOT, default apps/h2database/h2)
apps/netty-suite-runner/  apps/hib-suite-runner/  apps/keycloak-suite-runner/ …
```

`run-tomcat-ab.ps1` at the repo root points at a prebuilt binary and a Tomcat
class list. `azure-host-2` carries nine JDK images and is where the heavy suites
have historically run — check whether the workloads live there before fetching
anything locally.

**Decide and record which host you are on before measuring anything**, because
the timing-sensitive results from this host are not comparable with that one.

> **ANSWERED.** They live there. See the banner.

## 3. The instrument, and its three traps

```bash
cratonvm --java-home "$JDK" --jdk-only --explain-jdk-only \
         --jdk-only-report C:/windows/shaped/path/report.json \
         -cp <cp> <Main>
```

Working screen: **`probes/dodscreen.sh`**, which already encodes all three:

* a dump/report flag placed **after** the main class is silently ignored — no
  file, no warning, exit 0;
* the report path must be **Windows-shaped** on this host, or the VM prints
  `os error 3`, continues, and the file never appears;
* the report is **not written when the program calls `System.exit`** — which a
  servlet container or a Spring Boot app very well may. Probes here grew a
  `-Dprobe.noexit=1` escape for exactly this; you may need the same for a real
  application.

A fourth, learned the hard way today: the regression suite writes its own census
to a PID-scoped directory and **deletes it at the end**, and passing your own
`--jdk-only-report` makes the suite skip its census and point every vector at
your single path. If you want a corpus-wide census you need a per-vector path,
not one shared one.

> **A fifth, on Linux:** `--Xmx` takes its size as a separate argument.
> `--Xmx2g` is an argument-parsing error, exit 2, no report written — which
> looks exactly like the `System.exit` case. `probes/dodscreen-linux.sh` is the
> Linux runner and prints `NO-REPORT-WRITTEN` instead of letting it pass.

## 4. What the screen already says, at the scale available

`the-definition-of-done-screen-run-for-the-first-time-20260828.md`. On five
probe programs under `--jdk-only`:

```text
compatibility_classes        0   on ALL FIVE   <- the DoD predicate
synthetic_stub_invocations   0   on ALL FIVE
fabrication requests         4 distinct, all named with a requester file:line
```

**A request is not a failure.** The native asks, is correctly refused, and the
caller recovers onto real JDK bytecode — that is strict mode working. The
blocking set is the intersection: *refused AND not recovered from*. Three of the
four recover, proven by the probe rows rather than asserted. The fourth is the
FFM segment identity (§4 of that page), closest to L1.

**This is not the definition of done being met**, and the page says so in its own
section rather than letting the zeros read as the finish line. Your job is to run
the same instrument on the three real workloads.

## 5. What "done" looks like for L7

1. The three workloads run to completion under `--jdk-only` on a named host.
2. A report from each, screened on `compatibility_classes` and on the
   `compatibility-class-requested` rows — **by row, not by prefix**.
3. For every fabrication request: name it, name its requester `file:line`, and
   show whether the caller recovered. Recovery must be demonstrated by the
   workload's own result, not asserted.
4. A record with the numbers, the host, and the exact commands.

If a workload cannot be run here, **say so and name what is missing** — a
recorded blocker is worth more than a proxy measurement presented as the real
thing, and the whole reason this page exists is that the zeros in §4 are not the
answer to the question the roadmap asks.

> **All five discharged.** See the record named in the banner.
