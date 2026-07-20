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

## 2026-07-19 investigation: two real findings, neither explains this test's slowness

Added `CRATONVM_DBG_JIT_METHOD_STATS=1` (`jit/src/tiered.rs`'s
`dump_method_stats_to_stderr`, dumps per-method invocation-vs-promotion
counts + which methods crossed the JIT threshold but never compiled, at
process exit) and used it to profile `testSlicesSparseWithFilter` (the same
test class, ~86-150s, a much faster proxy than the 600s+ `testSlicesDense`
itself).

**Finding 1** (real, but a dead end for this doc): of 1345 invoked methods,
967 (72%) crossed the JIT `c1_threshold` (1500 invocations) but never
compiled — every one showed `tier_fail_count=3` (permanently gave up) and
`queued=false`. Root cause: `vm/src/jit/skip_list.rs`'s blanket
`org/apache/lucene/*` ban (`LUCENE-POSTINGS.1`), which force-interpreted
essentially the entire Lucene surface these tests run through (`Sorter`,
`Automaton`, `BytesRef`, `ByteArrayDataInput`, `DirectReader`,
`Lucene90DocValuesProducer`, etc.) — a deliberate, documented
correctness-driven ban (a JIT-vs-postings corruption bug), not a bug in the
tiering mechanism itself.

**Investigated whether that ban was still needed — ban stays, but not for
the reason initially thought.** Re-ran the ban's own original repro plus
much broader coverage (see `vm/src/jit/skip_list.rs`'s `LUCENE-POSTINGS.1`
comment for the full verification log) with the ban lifted: zero
corruption across ~1400s of Lucene-JIT-compiled execution, and (separately)
lifting it did NOT meaningfully speed up `testSlicesDense` (602.591s vs.
602.662s interpreted — noise-level, both cut short by the test's own 580s
suite timeout). Based on that evidence the ban was removed and merged with
same-day `origin/dev` commits — but the very next verification run,
immediately post-merge, hit a NEW `EXCEPTION_STACK_OVERFLOW` crash in
`GenerationalHeap::get_field`. **Turned out to be unrelated to this whole
investigation**: confirmed the SAME crash reproduces on a byte-for-byte
clean, unmodified `origin/dev` build with the ban fully in place (default
config) — a genuine, pre-existing `dev` regression that had nothing to do
with Lucene/JIT, just discovered by coincidence while testing it. **Since
FIXED** (same session, root cause: an unrelated `Path.toString()` native
infinite-recursion bug in `native-builtins/src/phases_late.rs` — see
[`ES-CRASH-20260719-lucene-jit-getfield-stack-overflow-FIXED.md`](../../internal/elasticsearch-suite/ES-CRASH-20260719-lucene-jit-getfield-stack-overflow-FIXED.md)
for the full writeup). The Lucene ban itself was left in its original
(banned) state regardless, since lifting it never showed a performance
benefit for this test — no reason to carry the extra unproven-safety risk.
**This specific performance lead is closed** (the ban was never the
dominant cost driver for `testSlicesDense`, so there's no more upside in
chasing it further here).

**Finding 2** (not investigated further, real risk if touched): the
*separate* `java/util/*` package ban in the same skip list (a documented
hash-table-loop regalloc miscompile, unrelated to `LUCENE-POSTINGS.1`) also
force-interprets hot JDK collection methods this test uses heavily
(`Arrays.rangeCheck`, `BitSet.wordIndex`/`get`, `Objects.checkIndex`,
`Arrays.compareUnsigned`). This ban was **not** re-verified or lifted —
unlike the Lucene ban, it guards a different, still-real miscompile, and
touching it needs its own dedicated investigation, not an afterthought here.

**Status of the actual performance question: still open.** Both leads
investigated this session turned out not to be the dominant cost. Whoever
picks this up next should not re-litigate the Lucene-ban question (closed,
see above) — either investigate the `java/util/*` ban's actual impact on
this test (carefully — it protects a real correctness bug), or do the
`CRATONVM_DBG_JIT_METHOD_STATS=1` profiling pass directly against
`testSlicesDense` itself (not just the faster `testSlicesSparseWithFilter`
proxy) to see whether the hot-but-stuck-interpreter picture looks
meaningfully different once the Lucene ban is out of the way, or whether
raw interpreted-bytecode throughput on the surviving `java/util/*`-banned
methods (or something else entirely — GC, I/O, algorithmic cost) now
dominates.

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
