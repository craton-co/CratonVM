# tests/base: DependencyGraphResolver.scan() spin loop — FIXED (upstream Keycloak source, not a CratonVM change)

Status: FIXED — but the fix lives in the vendored Keycloak checkout's Java source
(`apps/keycloak/`, gitignored/untracked by CratonVM's own git history), not in
CratonVM itself. Confirmed 2026-07-06 as almost certainly an upstream Keycloak logic
bug, not a CratonVM defect; fixed 2026-07-07.

Date observed: 2026-07-06.

## Original symptom

At least 3 `tests/base` classes hit the full 1200-second timeout (`HANG`) with stderr
logs dominated by tens of thousands of repeats of
`TRACE [org.keycloak.testframework.injection.DependencyGraphResolver] Skipping {0} already scanned`
(`AccountRestServiceLightweightTokenTest` 76,688 repeats, `LoginTest` 58,249,
`JWTAuthorizationGrantTest` 19,902) — roughly 60-65 repeats/second sustained for the
full 20-minute timeout, a genuine tight loop.

## Root cause

Found directly in Keycloak's own source,
`test-framework/core/src/main/java/org/keycloak/testframework/injection/DependencyGraphResolver.java`,
method `scan(Dependency)`: the `if (visited.contains(dependency))` branch only logged
"already scanned" — it never `return`ed. Every re-visit of an already-visited
dependency (normal for any DI graph with shared/diamond dependencies) re-resolved the
matching instance, re-fetched its declared dependencies, and recursed into
`dependencies.forEach(this::scan)` again, re-processing that entire subtree from
scratch. Finite (bounded by the graph's structure, no true cycle) but combinatorially
blown up by "width" — HotSpot's raw throughput hides this in a few seconds; CratonVM's
current throughput gap on CPU-bound recursive/stream-heavy work turns the same
finite blowup into a multi-hundred-thousand-call storm that exceeds even a
1200s timeout. This is a pure-Java logic bug independent of which JVM runs it —
verified to reproduce on real HotSpot too, just fast enough there to go unnoticed.

## Fix

Added the missing `return;` right after the "already scanned" log line in
`apps/keycloak/test-framework/core/src/main/java/org/keycloak/testframework/injection/DependencyGraphResolver.java`,
then recompiled and reinstalled the module (`./mvnw.cmd -pl test-framework/core install
-DskipTests -o` from `apps/keycloak`) so the fix takes effect for any suite run pointing
`-KeycloakRoot` at that checkout.

**Note for future sessions:** `apps/keycloak` is a local Keycloak checkout excluded from
CratonVM's own git tracking (`.gitignore` line 10: `apps/`). This fix does not travel
with the CratonVM repo — any *other* Keycloak checkout used as a suite-runner fixture
(fresh clone, a different worktree's `apps/keycloak-fresh`, the Azure host's checkout,
etc.) needs this same one-line patch re-applied (or upstream Keycloak needs to actually
fix it) before this class of hang goes away there too. Also worth reporting upstream to
Keycloak directly, since it benefits every JVM.

Verified:
- `JWTAuthorizationGrantTest` via the suite runner: before fix, part of the 1200s-timeout HANG set; after fix, completes in 119.3s (was hitting the full 1200s timeout). "Already scanned" line count dropped from 19,902/20,685 total to 4,847/5,690 total — consistent with normal DAG re-visit logging (each dependency can still be "skipped" once per parent) rather than the prior cascading re-traversal.
- The class now fails fast for a distinct, already-documented, already-confirmed-not-a-CratonVM-bug reason: [`keycloak-test-framework-remote-providers` Maven artifact resolution NPE](keycloak/keycloak-tests-base-remote-providers-artifact-resolution-NOT-A-BUG.md) (confirmed 2026-07-07 to reproduce identically on real HotSpot) — not a repeat of the spin-loop hang.

## Also noted in passing (not fixed, low priority)

JBoss-Logging `tracev`/`logv` (indexed/`MessageFormat`-style parameterized logging)
doesn't substitute its `{0}`/`{1}` arguments under CratonVM — literal tokens print
instead of values. Affects readability/diagnostics only, not correctness. Not chased
further as part of this fix.

## Evidence

- Source fix: `apps/keycloak/test-framework/core/src/main/java/org/keycloak/testframework/injection/DependencyGraphResolver.java` (add `return;` after the "already scanned" log).
- Verification run: `apps/keycloak-suite-runner/.suite/results/verify-depgraph-fix-20260707/`.
