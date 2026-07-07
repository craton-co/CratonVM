# tests/base: DependencyGraphResolver.scan() re-scans already-visited dependencies without returning, causing a massive spin loop / hang

Status: open (as a CratonVM-observable timeout) — but root cause is almost certainly an upstream Keycloak logic bug (missing early `return`), not a CratonVM defect. CratonVM's role is very likely limited to being slow enough to turn a large-but-finite redundant traversal into a full 1200s timeout. Filing this because it's a genuine, reproducible HANG under this harness/timeout — not a claim that CratonVM itself is broken.

Date observed: 2026-07-06 (1200s-timeout rerun, branch fix/keycloak-nonpassed-rerun-1200s-20260706)

## Summary

At least 2 `tests/base` classes hit the full 1200-second timeout (`HANG`,
`rc=TIMEOUT`) with near-identical stderr logs dominated by tens of thousands
of repeats of the same trace line:

- `org.keycloak.tests.account.AccountRestServiceLightweightTokenTest` — err
  log has 77,513 total lines, **76,688** of which are
  `TRACE [org.keycloak.testframework.injection.DependencyGraphResolver] Skipping {0} already scanned`.
- `org.keycloak.tests.forms.LoginTest` — err log has 59,024 total lines,
  **58,249** of which are the same line.
- `org.keycloak.tests.oauth.JWTAuthorizationGrantTest` (found independently
  2026-07-07, local-host rerun, different branch/worktree/binary) — err log
  has 20,685 total lines, **19,902** of which are the same line.

That's roughly 60-65 repeats *per second* sustained for the full 20-minute
timeout in the first two cases — a genuine tight loop, not merely "slow."
Three independent classes, two separate hosts/sessions, same exact
signature — this is a solid, reproducible finding.

## Root cause — found directly in Keycloak's own source, full method read

`test-framework/core/src/main/java/org/keycloak/testframework/injection/DependencyGraphResolver.java`
(full `scan()` method, plus the constructor that seeds it):

```java
public DependencyGraphResolver(Registry registry) {
    this.registry = registry;
    this.missingInstances = new LinkedList<>();

    for (RequestedInstance requestedInstance : registry.getRequestedInstances()) {
        List<Dependency> dependencies = requestedInstance.getSupplier().getDependencies(requestedInstance);
        requestedInstance.setDeclaredDependencies(dependencies);
        for (Dependency dependency : dependencies) {
            scan(dependency);
        }
    }
}

private void scan(Dependency dependency) {
    if (visited.contains(dependency)) {
        log.tracev("Skipping {0} already scanned", dependency);
    } else {
        log.tracev("Scanning dependency {0}", dependency);
    }
    // <-- no `return` here when already visited; execution falls through regardless

    if (visiting.contains(dependency)) {
        throw new RuntimeException("Dependency cycle detected in " + ...);
    }

    visiting.add(dependency);

    RequestedInstance matchingInstance = registry.getRequestedInstances().stream()
        .filter(RequestedInstancePredicates.matches(dependency.valueType(), dependency.ref()))
        .findFirst().orElse(null);
    if (matchingInstance == null) {
        matchingInstance = missingInstances.stream()
            .filter(RequestedInstancePredicates.matches(dependency.valueType(), dependency.ref()))
            .findFirst().orElse(null);
    }
    if (matchingInstance == null) {
        // creates a NEW missing-instance registration EVERY time this
        // dependency is (re-)scanned, even after it was already resolved
        Supplier<?, ?> supplier = registry.getExtensions().findSupplierByType(dependency.valueType());
        Annotation defaultAnnotation = DefaultAnnotationProxy.proxy(supplier.getAnnotationClass(), dependency.ref());
        matchingInstance = registry.createRequestedInstance(new Annotation[]{ defaultAnnotation }, dependency.valueType());
        missingInstances.add(matchingInstance);
    }

    List<Dependency> dependencies = matchingInstance.getSupplier().getDependencies(matchingInstance);
    matchingInstance.setDeclaredDependencies(dependencies);

    dependencies.forEach(this::scan);   // <-- recurses into the SAME sub-dependencies again on every re-visit

    visiting.remove(dependency);
    visited.add(dependency);
}
```

The `if (visited.contains(dependency))` branch only logs — it never
`return`s. So every time `scan()` is invoked on an already-visited
dependency (which happens naturally any time two or more suppliers share a
common dependency — completely normal for a DI graph, and NOT a cycle the
`visiting.contains()` check would catch, since `visiting.remove()` already
ran by the time the second path reaches it), it:
1. Re-resolves (or worse, re-creates a duplicate `missingInstances` entry
   for) the matching instance,
2. Re-fetches that instance's own declared dependencies, and
3. Recurses into `dependencies.forEach(this::scan)` — re-processing that
   entire subtree from scratch.

For a graph with nested diamond-shaped sharing (a shared dependency whose
own dependencies are *also* shared elsewhere), this compounds: each
re-visit re-triggers a full re-traversal of everything beneath it, which is
finite (bounded by the graph's actual structure — no true cycle, so it does
terminate eventually) but can blow up combinatorially with graph "width."
This exactly matches the observed evidence: two different test classes
produced two different large-but-distinct scan-call counts (76,688 vs
58,249), consistent with each class's own dependency graph shape driving
its own blowup factor, not a fixed infinite loop.

This is a genuine defect in **Keycloak's own test-framework source** (a
missing early-return after detecting an already-visited dependency) — a
pure Java logic bug, independent of which JVM runs it.

## Conclusion: very likely NOT a CratonVM-specific bug, but a real upstream Keycloak defect that CratonVM's current performance profile turns into an outright timeout

Because the redundant work stems from ordinary Java control flow (a
missing `return`, ordinary `HashSet`/stream operations, ordinary recursion)
with no VM-specific API involved, this bug's *existence* should reproduce
identically on real HotSpot. What's uncertain (not yet tested) is whether
HotSpot's much higher raw throughput completes the same blown-up call count
in a few seconds (so upstream Keycloak's test suite never notices this bug
in practice), while CratonVM's currently lower throughput turns that same
finite amount of work into something that exceeds even a 1200-second
timeout. Given CratonVM's documented throughput gap versus HotSpot on
CPU-bound work elsewhere in this project (e.g. bt18 ~34x, fib workloads
3.7x-28x depending on optimization state), a graph blowup that costs
HotSpot single-digit seconds could plausibly cost CratonVM 20+ minutes —
this is a fully plausible mechanism, not a stretch.

Practical takeaway: this is a genuine, reproducible HANG for these 2 test
classes under CratonVM at the current 1200s timeout — worth keeping open
as a CratonVM-observable failure — but the *root cause and fix* belong
upstream in Keycloak's `DependencyGraphResolver.scan()` (add
`return;` right after the "already scanned" log line), not in CratonVM.
If CratonVM's throughput on this pattern later improves enough that these
classes complete within the timeout, that would independently confirm this
diagnosis without ever needing a HotSpot comparison run.

## Next steps

1. (Optional, would fully settle the question) Run either class under real
   HotSpot with the same harness and a generous timeout (a few minutes) to
   directly confirm it also does the same redundant work, just fast enough
   to finish. Not required to act on this finding, since the mechanism is
   already understood from source inspection alone.
2. Consider reporting the missing `return` upstream to Keycloak — it's a
   one-line fix (`if (visited.contains(dependency)) { log.tracev(...);
   return; }`) that would benefit every JVM, not just CratonVM.
3. If CratonVM throughput on this recursive/stream-heavy pattern is later
   investigated as its own project (separate from "is this a bug"), this
   test class pair is a ready-made, real-world benchmark for it.

## Repro

```
ssh -i "C:\Users\Victor\.ssh\azure.pem" -o IdentitiesOnly=yes victor@20.83.144.174
cd /data/data/wt-keycloak-nonpassed-1200-20260706   # or the corrected /data/data/data/... path if the host's duplicate-mount issue recurs
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/keycloak-suite-runner/run-keycloak-suite.ps1 -Vm craton \
  -ClassList <(printf 'module\tclass\ntests/base\torg.keycloak.tests.forms.LoginTest\n') \
  -TimeoutSec 60 -RunName repro-depgraph-spin \
  -KeycloakRoot apps/keycloak-fresh \
  -Exe target/release/cratonvm-nonpassed1200-20260706 -JdkHome /home/victor/jdk25
# Expect the .err.log to fill with repeated "Skipping {0} already scanned" lines within the first few seconds.
```

## Also noted in passing (separate, smaller finding — not filing its own doc)

The same log lines show **unsubstituted JBoss-Logging `tracev`/`logv`
placeholders**: e.g. `INFO [testinfo] {0} - {1}` (from
`org.keycloak.testframework.LogHandler.logDivider`/`logTestClassStatus`,
which call `LOGGER.logv(level, "{0} - {1}", ...)`) prints the literal `{0}`/
`{1}` tokens instead of the substituted argument values, and likewise
`Skipping {0} already scanned` never substitutes the actual `Dependency`
object's `toString()`. This suggests CratonVM's JBoss-Logging
`tracev`/`logv` (indexed/`MessageFormat`-style parameterized logging, as
opposed to the SLF4J-style `{}` placeholders) isn't performing parameter
substitution correctly. Worth a dedicated look since this affects any
log line that's genuinely useful for debugging (every `tracev`/`logv` call
site loses its actual argument values under CratonVM) — but it's a
readability/diagnostics gap, not a functional/correctness bug on its own,
so it doesn't need a separate doc unless someone wants to chase it
specifically.

## Evidence

`/data/data/wt-keycloak-nonpassed-1200-20260706/apps/keycloak-suite-runner/.suite/results/nonpassed1200-20260706-shard1/others-jit/logs/tests_base.org.keycloak.tests.account.AccountRestServiceLightweightTokenTest.err.log` (77,513 lines) and
`.../nonpassed1200-20260706-shard2/others-jit/logs/tests_base.org.keycloak.tests.forms.LoginTest.err.log` (59,024 lines), both from the 2026-07-06 4-shard rerun with 1200s timeout. Source: `apps/keycloak-fresh/test-framework/core/src/main/java/org/keycloak/testframework/injection/DependencyGraphResolver.java` lines 41-52 (fresh clone of Keycloak `main` @ commit `8160276`).
