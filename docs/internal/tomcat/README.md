# Tomcat suite — known issues index

Split out of `../fixed-suite-bugs/tomcat/18-fixture-environment-gaps-20260724.md`
(2026-07-24) into one file per independently-actionable item, so different
sessions can pick separate items up in parallel without stepping on each
other. Source data and the full 35-class categorization with root-cause
detail: that doc, plus `../../../apps/tomcat-suite-runner/RESULTS-20260724-cwdfix.md`
on the Azure host. All of this is against the Linux Tomcat fixture at
`/data/data/tomcat-dohead-fixture-20260717` (symlinked
`/data/data/apps/tomcat`), reusable Linux runner at
`../../../apps/tomcat-suite-runner/run-tomcat-suite.sh`.

## Fixture-completion work — ALL 6 IMPLEMENTED 2026-07-23

| Doc | Classes | Outcome |
|---|---:|---|
| [missing-antjar-classpath.md](missing-antjar-classpath.md) | 2 | ✅ Fixed — 1 PASS both, 1 revealed a real regression |
| [missing-httpd-binary.md](missing-httpd-binary.md) | 8 | ✅ Fully fixed — all 8 PASS both VMs, no regressions |
| [largeheap-flat-heap-oom.md](largeheap-flat-heap-oom.md) | 3 | ⚠️ Partial — 2 now PASS HotSpot/reveal regressions, 1 still fails both (narrower) |
| [missing-catalina-localhost-context-configs.md](missing-catalina-localhost-context-configs.md) | 8 | ⚠️ Root-cause theory was wrong (see doc) — real fix was the lib-jars doc below; 2 PASS both, 6 revealed regressions |
| [missing-build-lib-jars.md](missing-build-lib-jars.md) | 1 | ✅ Fixed via `ant deploy` — also fixed most of the "conf/Catalina/localhost" bucket above |
| [unbuilt-virtual-webapp-submodule.md](unbuilt-virtual-webapp-submodule.md) | 1 | ⚠️ Root-cause theory was wrong (no Maven module) — one method now passes, a second method reveals a narrower regression |

As predicted, completing these turned several into real CratonVM
regressions rather than clean passes — see
[20-fixture-completion-regressions-closure-FIXED.md](../fixed-suite-bugs/tomcat/20-fixture-completion-regressions-closure-FIXED.md)
(moved to `..` 2026-07-24, superseding the now-closed
`regressions-revealed-by-fixture-completion-20260723.md`) for the full
accounting: of the 9 confirmed CratonVM-only regressions, **7 are fixed and
verified**; the remaining 2 (`TestManagerWebapp.testBug57700`,
`TestSsl.testPost`) are confirmed to be the same already-tracked,
deliberately-deferred interpreter/dispatch throughput ceiling as
[04-embedded-server-throughput-wall-OPEN.md](../../known-issues/tomcat/04-embedded-server-throughput-wall-OPEN.md),
not new or independently-fixable bugs.

Note: none of this fixture work is git-tracked — it all lives on the Azure
host (`/data/data/apps/tomcat`, i.e. `/data/data/tomcat-dohead-fixture-20260717`).
A future session rebuilding this fixture from scratch needs to redo these
steps (see each doc's "RESOLVED" note for the exact commands).

## Untriaged oddities — RESOLVED 2026-07-23

Both classes formerly tracked here (`org.apache.catalina.startup.TestTomcat`'s
misleading "Deliberately Broken" log line, and
`org.apache.jasper.compiler.TestNonstandardTagPerformance`'s self-referential
`ClassNotFoundException`) are fully triaged and closed — see
[19-untriaged-oddities-closed-shared-hashtable-bug-FIXED.md](../fixed-suite-bugs/tomcat/19-untriaged-oddities-closed-shared-hashtable-bug-FIXED.md)
in `../fixed-suite-bugs/tomcat`. Short version: "Deliberately
Broken" was always a red herring (from tests that deliberately trigger and
catch it); the real bug underneath was a genuine CratonVM regression — a
`java.util.Hashtable` field-misresolution bug that silently doubled
`Hashtable.size()` on every `put()`, which corrupted Jasper's embedded ECJ
Java compiler (JSPs use a `Hashtable` internally) and broke JSP compilation
entirely. Now fixed; `TestTomcat` is 26/26 PASS. The
`TestNonstandardTagPerformance` class was a fixture-data typo (missing "er"
in `.suite/all-tests.txt`) with no code fix needed.

**New untriaged item (2026-07-24):** `org.apache.catalina.nonblocking.TestNonBlockingAPI`
fails on *both* CratonVM and HotSpot in this fixture for a reason that has
never been pinned down — surfaced while root-causing this class's separate,
CratonVM-only `value_stack.rs` panic (see below), which is now fixed and
unrelated to this failure. Needs a future session to run this class against
both VMs, diff the actual failing assertion/exception, and categorize it.

## Fixed and moved to `..`

| Doc | Classes | Outcome |
|---|---:|---|
| [hang-classification-unconfirmed-host-contention-FIXED.md](../fixed-suite-bugs/tomcat/hang-classification-unconfirmed-host-contention-FIXED.md) | 9 | ✅ HANG was a pure host-contention artifact (HotSpot passes all 9 cleanly); once ruled out, all 9 were genuine CratonVM regressions from 2 root causes (ecj/Hashtable JSP-compile NPE affecting 8; SSLContext-resolution-through-wrapped-factory affecting `TestCustomSsl`), both fixed and verified — 9/9 PASS on CratonVM on a quiet host |
| [20-fixture-completion-regressions-closure-FIXED.md](../fixed-suite-bugs/tomcat/20-fixture-completion-regressions-closure-FIXED.md) | 9 | ✅ 7/9 fixed (X509Certificate toString, symlink+canonicalize, G1 for `*LargeHeap`, catchable-OOME Cipher fixes, 2 TestSsl bugs); 2 residual confirmed = pre-existing throughput ceiling, not new bugs |
| [value-stack-usize-underflow-nio-worker-panic-FIXED.md](../fixed-suite-bugs/tomcat/value-stack-usize-underflow-nio-worker-panic-FIXED.md) | 2 | ✅ Fixed — `usize` underflow panic on a background NIO worker thread (`TestNonBlockingAPI` / `TestWebSocketFrameClientSSL`, both hitting `LinkedBlockingQueue.take()`'s `Condition.await()` interface dispatch) root-caused to a missing pre-pop deopt-frame snapshot in `../../../jit/src/x64.rs`'s generic invoke-dispatch codegen; verified panic-free across 35 repro attempts |

## Retired from `../../known-issues/tomcat/` 2026-09-12 (c1/c2 JIT-tier suite run)

| Doc | Classes | Outcome |
|---|---:|---|
| [cp-txt-stale-gradle-module-cache-paths-FIXED-20260912.md](cp-txt-stale-gradle-module-cache-paths-FIXED-20260912.md) | 4 | ✅ Fixture — `cp.txt` named six jars a Gradle cache had evicted. The Windows harness now pins Tomcat's own versions into `.suite/lib/pinned` and checks the classpath before every run; all 4 PASS on both VMs |
| [easymock-bytebuddy-classpath-version-gap-FIXED-20260912.md](easymock-bytebuddy-classpath-version-gap-FIXED-20260912.md) | 8 | ✅ Fixture — byte-buddy 1.14.12 picked by glob; with Tomcat's pinned 1.18.8 all 8 EasyMock classes PASS on both VMs, HotSpot included |
| [c1-c2-single-arm-timing-sensitive-flakes-RESOLVED-20260912.md](c1-c2-single-arm-timing-sensitive-flakes-RESOLVED-20260912.md) | 5 | ✅ `TestAccessLogValve` was a real CratonVM defect, not a flake: native `Stream.distinct()` and `Set.of` duplicate checks were all-pairs `equals` scans, costing the first `Calendar.getInstance` per locale 1.5 s — both hash-bucketed. The other 4 re-measured standalone in both arms |
