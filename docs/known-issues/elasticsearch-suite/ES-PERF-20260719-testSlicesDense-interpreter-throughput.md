# ES PERF — `testSlicesDense` (IVFKnn) is genuinely slow under CratonVM, not hung or corrupt

Status: OPEN (performance only — not a hang, not a correctness bug)

Split out from
[`ES-HANG-20260709-server-org-elasticsearch-search-vectors-diversifyingchildrenivfknnfloatslicedvectorquerytests-3ff8aa1c4b.md`](../../internal/elasticsearch-suite/ES-HANG-20260709-server-org-elasticsearch-search-vectors-diversifyingchildrenivfknnfloatslicedvectorquerytests-3ff8aa1c4b-FIXED.md)
(archived to `docs/internal/` 2026-07-19: every hang/corruption/crash bug that
doc tracked across its 2026-07-09 through 2026-07-19 history is now fixed —
see that doc for the full investigation trail). This doc exists solely to
keep tracking the one thing in that history that was never a bug: raw
interpreter throughput on this specific test.

## Class / method

`org.elasticsearch.search.vectors.DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests#testSlicesDense`

## Symptom

The test takes on the order of 600+ seconds under CratonVM (vs. ~13.5s
under real HotSpot for the same selection). It is making genuine forward
progress the whole time — not deadlocked, not spinning uselessly, not
corrupting data — it is just slow. Most runs either complete in ~600s or
get cut off by the *test framework's own* `-Dtests.timeoutSuite` (commonly
set to 580000ms in repro commands), which reports a clean
`Test abandoned because suite timeout was reached` / `Suite timeout
exceeded` JUnit failure — not a VM hang, not an external watchdog abort.

## Evidence this is throughput, not a hang

- An unbounded run (no watchdog) completed in ~602s, stopped by the test
  framework's own suite timeout, not by CratonVM.
- 2026-07-19 re-verification (post `Thread.join()` lost-wakeup fix, see the
  archived doc): `Time: 602.662`, process exits cleanly via `System.exit(1)`
  with a proper JUnit report (2 failures, both suite-timeout-shaped) —
  confirms this is the framework timeout being hit on schedule, not a
  process that never returns.
- A live gdb snapshot during one run showed deep `Method.invoke()`
  reflection chains and `local_liveness::analyze` cache misses dominating —
  consistent with interpreter/JIT overhead on a reflection- and
  exception-handling-heavy code path, not a stuck lock or a corrupted
  data structure.
- Every correctness-shaped hypothesis investigated across this cluster's
  history (STW/monitor race, GAP_FILLER_CLASS_ID young-GC walk truncation,
  guarded-inline-getfield SIGSEGV, array-constructor-reference lambda bug,
  stale-precise-root-mirror JIT race, `Thread.join()` lost wakeup) turned
  out to be a real, fixed bug **elsewhere** in the suite (affecting other
  tests/classes too) — none of them, once fixed, changed `testSlicesDense`'s
  fundamental ~600s wall-clock time.

## Not yet investigated

Nobody has done a dedicated profiling pass aimed at *reducing* this time —
all prior sessions were chasing (and fixing) correctness bugs that
happened to surface via this same slow test, not optimizing the interpreter
path itself. A real investigation would want: a flamegraph/sampling
profile of a full `testSlicesDense` run, isolating how much of the ~600s
is reflection dispatch (`Method.invoke`) vs. `local_liveness::analyze`
cache misses vs. actual Lucene/IVFKnn indexing work, and whether JIT
compilation thresholds are even being hit for the hot methods on this path
given the test's short per-call-site iteration counts.

## Repro

```powershell
$JDK = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
$ES  = "C:\craton\CratonVM\apps\elasticsearch"
$CP  = (Get-Content "$ES\server\build\craton-testcp.txt" | ForEach-Object { $_.Trim() } | Where-Object { $_ }) -join ';'
& $EXE --java-home $JDK --stack-dump-on-timeout 750 --Xmx 2g `
  -Dtests.seed=B17AC9D3E1F2A0C4 -Des.path.home=$ES `
  -Dtests.testfeatures.enabled=true -Dtests.security.manager=false -Dtests.asserts=false `
  -Dtests.timeoutSuite=580000! -Dtests.method=testSlicesDense `
  <standard ES --add-opens set, see run-elasticsearch-suite.ps1 Get-EsJavaArgs> `
  -cp $CP org.junit.runner.JUnitCore `
  org.elasticsearch.search.vectors.DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests
```

Expect `Time: ~600s` and a suite-timeout-shaped JUnit failure (or `OK` if
`-Dtests.timeoutSuite` is raised past ~610000). Use a `--stack-dump-on-timeout`
value comfortably above 600s if you want to rule out a real hang rather than
just observing the expected slow completion.
