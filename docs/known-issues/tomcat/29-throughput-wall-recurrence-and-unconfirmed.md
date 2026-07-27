# Throughput-wall recurrence, relative-performance-assertion family, and unconfirmed findings

Not new bugs — grouped here for completeness of this rerun's accounting,
distinct from the 8 genuinely new, well-isolated bugs in the sibling docs
(21-28) in this batch.

## Already-known, OPEN "embedded-server throughput wall" reconfirmed at larger scale

`04-embedded-server-throughput-wall-OPEN.md` already documents CratonVM's
per-request/per-deploy interpreter overhead vs. HotSpot. This 1500s rerun
reconfirms it compounds badly in classes with many sequential embedded-
server start/stop or deploy/undeploy cycles — evidence, not new:

- `org.apache.catalina.startup.TestHostConfigAutomaticDeploymentAddition` —
  log shows a single webapp directory deploy taking **107.9 seconds**; the
  class does several such cycles across its test methods, exceeding even a
  1500s budget.
- `org.apache.catalina.startup.TestHostConfigAutomaticDeploymentModification` —
  a single deployment-descriptor deploy takes **109.7 seconds**; same
  pattern.
- `org.apache.catalina.startup.TestHostConfigAutomaticDeploymentDeleteC` —
  still hangs at 1500s in both runs (its sibling `TestHostConfigAutomaticDeploymentDeleteB`
  passed at 1365s in the second run — right at the edge, confirming this is
  a matter of degree, not a qualitatively different defect).
- `org.apache.catalina.startup.TestHostConfigAutomaticDeploymentUnpackWAR` /
  `...UpdateWarOffline` — both eventually FAIL (not hang) after 890-962
  seconds — worth a closer look at whether the failure is itself throughput-
  induced (a client-side timeout embedded in the test) or a separate
  assertion, but not investigated further here.
- `org.apache.coyote.http2.TestHttp2Section_8_2` — a single parameterized
  test class with 1000+ sub-cases (`testFieldNameAndValue[1121: ...]` was
  seen mid-run), each starting and stopping its own embedded connector —
  same multiplication effect, not a deadlock.

## Relative-performance-assertion family — likely same throughput ceiling, different assertion style

These assert "optimized path X is faster than naive/alternate path Y" rather
than a fixed wall-clock budget — CratonVM's JIT not yet matching HotSpot's
relative speedup between the two paths fails the assertion even where
absolute throughput isn't otherwise a functional problem:

- `org.apache.catalina.mapper.TestMapperPerformance.testPerformance` —
  `AssertionError: 40823` (a raw elapsed-ms or count value failing a
  threshold check).
- `org.apache.juli.TestOneLineFormatterPerformance.testDateFormat` —
  `AssertionError: String#format was faster that DateFormatCache` (the cache
  is supposed to win).
- `org.apache.tomcat.websocket.server.TestAsyncMessagesPerformance.testAsyncTiming` —
  plain `AssertionError` (timing-relative, not inspected further).
- `org.apache.el.parser.TestELParserPerformance.testParserInstanceReuse` —
  `AssertionError: Using new ElParser() was faster then using
  ELParser.ReInit` (interestingly this PASSED in the second/post-merge run
  at 912s — inconsistent across runs, consistent with a
  borderline/contention-sensitive relative-timing assertion rather than a
  hard functional bug).

Distinct in kind from `23-charsetcache-pathological-slowdown.md` (that one
is a 3x slowdown of the supposedly-faster path — a real defect, not just
"not yet as fast as HotSpot").

> **Update 2026-07-27 — one member of this family is now root-caused, and it
> is NOT a diffuse interpreter ceiling.**
> `org.apache.tomcat.util.http.TestMethodPerformance` was chased down to two
> *named* JIT-admission gates that leave its entire hot path interpreted: the
> loop method is permanently OSR-denied by the RBC.7 `invokedynamic` ban
> (triggered by its trailing `println("…" + duration + "ns")` string-concats),
> and `StringCache.toString` is refused outright by the RBC.6
> exception-handler-safety gate (triggered by its `synchronized` block's
> javac-generated monitor handler). Full analysis, probe table, and fix
> directions: [30](30-hot-loop-jit-admission-bans-testmethodperformance-OPEN.md).
> Worth checking whether the other relative-performance-assertion classes
> above fail the same way — `CRATONVM_DBG_JITC=1 CRATONVM_DBG_RBC6=1` names
> the gate in one run.

## Unconfirmed / contention-suspected — do not treat as new regressions without a clean rerun

- **`org.apache.jasper.compiler.TestGenerator`** — FAILed with a clean NPE
  (`testBug56581`, `"result" is null`) in the first (300s-normal, pre-merge)
  run, but HANG at 1500s in the second (post-merge) run, never reaching that
  same test method. Given the Hashtable-size-doubling fix that landed on
  `dev` between the two runs is known to have broken Jasper/ECJ JSP
  compilation entirely (see `../../internal/fixed-suite-bugs/tomcat/19-untriaged-oddities-closed-shared-hashtable-bug-FIXED.md`),
  `testBug56581`'s NPE may already be fixed — the class just didn't get far
  enough to prove it in the post-merge run (many sequential embedded-server
  test methods ahead of it, same throughput-multiplication pattern as the
  HostConfig cluster above). Needs a focused single-class rerun with a very
  long timeout (or `-Start`/`-Count` to isolate just `testBug56581`) to
  confirm either way.
- **`org.apache.catalina.nonblocking.TestNonBlockingAPI`** — PASSED at 438s
  in the first run, HANGed at 1500s in the second run on the SAME class,
  same fixture, no code path obviously related to the intervening merge.
  This local Windows box had multiple OTHER concurrent build/test sessions
  running throughout both reruns (confirmed via `tasklist` showing several
  live `cargo`/`cratonvm` processes not belonging to this session) — the
  most likely explanation is host contention inflating this class's many
  per-test-method embedded-server cycles past 1500s, not a genuine
  regression introduced by the merge. Needs a rerun on a quiet host before
  concluding either way.

## Not a CratonVM bug at all — Windows-harness fixture gap (already known, already fixed on a different host)

- **`org.apache.catalina.ant.TestDeployTask`** — fails with
  `NoClassDefFoundError: org/apache/tools/ant/Task` on this Windows
  harness. This is the SAME missing-`ant.jar`-on-the-classpath fixture gap
  already found and fixed on the Azure Linux fixture (see
  `docs/known-issues/tomcat/missing-antjar-classpath.md`) — it just hasn't
  been fixed on THIS host's `apps\tomcat-suite-runner\.suite\cp.txt` yet.
  The actual `%20`-decoding CratonVM bug this class also exercises
  (`bug58086a`) IS already fixed on `dev` per
  `../../internal/fixed-suite-bugs/tomcat/20-fixture-completion-regressions-closure-FIXED.md` — this class simply
  never gets far enough to prove it on Windows because of the classpath
  gap. **Fix:** add `ant.jar`/`ant-launcher.jar` to this Windows harness's
  classpath the same way the Linux one was fixed, then rerun.
