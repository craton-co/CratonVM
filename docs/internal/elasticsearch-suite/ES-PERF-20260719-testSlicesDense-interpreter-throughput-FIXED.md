# ES PERF — `testSlicesDense` (IVFKnn) interpreter throughput — FIXED

Status: FIXED 2026-07-21. The global `org/apache/lucene/*` JIT ban was the
remaining throughput limiter, but lifting it exposed a real residual:
JIT-compiled `ACC_SYNCHRONIZED` methods had no implicit monitor
prologue/epilogue. `IndexWriter.doWait()` therefore called `Object.wait()`
without owning its monitor, producing `IllegalMonitorStateException` and
downstream postings corruption. The JIT now rejects synchronized methods at
every admission path; Lucene is otherwise JIT eligible and the blanket ban is
removed. Historical investigation notes below are retained for provenance.

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

**Finding 2, 2026-07-21 update: investigated, fixed, and CLOSED as a dead
end for this doc — same pattern as the Lucene ban.** The premise above was
wrong on two counts, found by actually reading `vm/src/jit/skip_list.rs`
instead of trusting its own module-doc summary table:

1. The description here ("a documented hash-table-loop regalloc
   miscompile") was describing a DIFFERENT, older, already-narrowed ban
   (the `T1.1.g` `is_known_miscompile` targeted list — `HashMap`
   put/get/resize, `WeakHashMap` iterators, the AQS/CLQ families — gated
   behind a GPR-local-homes allocator flag that's been default-OFF since
   2026-07-04). The ban actually force-interpreting `Arrays`/`BitSet`/
   `Objects`/every other `java/util/*` class for THIS test was a different,
   *later* rule: `HIB-LONGTAIL.1` (commit `94ecd110b`, 2026-07-15), added as
   a throughput workaround for an unrelated Hibernate H2/ANTLR test
   scenario. It banned `org/h2/*`, `org/antlr/v4/runtime/*`, **and**
   `java/util/*` (minus regex) as three independent unconditional terms —
   not "only when combined", despite the comment's framing — so it silently
   force-interpreted java.util in *every* test in the suite, ES included,
   regardless of whether H2/ANTLR were anywhere in the picture.
2. It was never actually a correctness guard to begin with: the Hibernate
   longtail's real root cause (see
   [`hib-generic-timeout-hang-longtail-resolved-20260715.md`](../../internal/fixed-suite-bugs/hib-generic-timeout-hang-longtail-resolved-20260715.md))
   was the executor-compatibility bridge returning placeholder
   `FutureTask`s, fixed in the SAME commit as the skip-list change. The
   java.util term was never load-bearing for that fix — and it had been
   silently breaking 7 of `skip_list.rs`'s own unit tests
   (`tier1_skip_list_no_blanket_java_util_ban` and friends, which predate
   `HIB-LONGTAIL.1` and assert the exact opposite) ever since it landed.

**Fix**: narrowed `HIB-LONGTAIL.1` to just `org/h2/` + `org/antlr/v4/runtime/`
(dropped the `java/util/` term entirely — dev commit `0bdd62344`). All 58
`skip_list` unit tests pass (7 previously broken, now fixed).

**But lifting it did not measurably speed up `testSlicesDense` either** —
same non-result as the Lucene ban. Direct `CRATONVM_DBG_JIT_METHOD_STATS=1`
profiling of the real `testSlicesDense` (not just the sparse proxy) confirms
why: with java.util JIT-eligible, its `hot_but_stuck_in_interpreter` list is
now **exclusively** `org/apache/lucene/*` methods (`AssertingLeafReader`,
`BlockPackedReaderIterator`, `ByteArrayDataInput`, `Lucene90DocValuesProducer`,
etc. — the same population Finding 1 already identified and closed). No
`java/util/*` method appears in the top 30 stuck methods once the ban is
lifted — the Lucene ban alone already accounts for the whole
force-interpreted population that matters at this test's scale.

**A real, independent bug WAS found and fixed along the way, also not the
dominant cost but worth fixing regardless**: profiling with the newly-lifted
java.util surface turned up `java/util/stream/MatchOps.makeInt` deoptimizing
6928 times in a single ~700s window (`CRATONVM_DBG_DEOPT=1`), every single
time with `reason=UnreachedCode action=MakeNotCompilable` — an internal
`invokedynamic` (lambda) site inside the method that `jit_scan` lowers to an
unconditional-deopt uncommon trap, so once JIT-compiled this factory method
deopts on *every* call, and the runtime give-up mechanism (`jit_skip_set` /
`mark_jit_bail_listed`) never actually stopped re-invocation (almost
certainly because callers' own inline caches keep jumping straight to the
already-primed, doomed raw entry point, bypassing the bail-list check —
same general class of gap flagged in
`docs/feature-designs/jit-local-exception-handlers.md`'s RBC.6 investigation
for the `VIRTUAL_DISPATCH_CACHE` fast path). Added a targeted permanent
skip-list entry for `MatchOps.{makeInt,makeRef,makeLong,makeDouble}`
(`StreamMatchOpsUncommonTrap`, dev commit `b416011bc`), same pattern as the
existing `Collections.indexedBinarySearch` (ES812) entry just above it in
the file.
This eliminated the deopt storm entirely (`deopts: 10604 → 1`,
`c2_bailouts: 10601 → 0`) but — consistent with the java.util finding above
— did not move `testSlicesDense`'s wall-clock time (822.7s before vs. 822.7s
after; the storm was real wasted work, just not enough of it relative to the
test's total cost to matter at this scale). Worth fixing on its own merits
(it's a genuine, previously-undiscovered JIT tiering bug — the give-up
mechanism not sticking), independent of this doc.

**2026-07-21 timing note — the `~600-602s` figure in this doc's "Symptom"
section above no longer reproduces on this host and should not be trusted as
a tight regression baseline.** Three same-day A/B/C measurements — this
session's fully-fixed binary, an unmodified current-`dev` binary, and a
binary built from the EXACT commit (`fa91ef9e5`) this doc's 2026-07-19
investigation used — all measured `testSlicesDense` at 822.6–823.2s (within
0.6s of each other). Since the doc-era commit itself no longer reproduces
602s today, the ~37% gap vs. the originally-documented figure is host/day
environmental variance (this is a heavily shared, multi-tenant Azure box —
see `azure-host-disk-full-flapping` / load-average notes elsewhere), **not**
a code regression from any of the ~40 commits that landed between
2026-07-19 and 2026-07-21. Do not git-bisect this gap; it was checked and
doesn't bisect (identical timing at both the old and new commit, measured
back-to-back on the same host state).

**Status of the actual performance question: still open, but both
originally-flagged leads (Lucene ban, java/util ban) are now conclusively
closed as non-dominant.** The `hot_but_stuck_in_interpreter` population is
consistently and exclusively `org/apache/lucene/*` across every
configuration tested (java.util banned or not, MatchOps deopting or not).
Whoever picks this up next should not re-litigate either ban (both
independently confirmed lifting them doesn't help) and should instead either
(a) profile the JIT-compiled `org/apache/lucene/*` code paths themselves for
whether the *compiled* code is throughput-bound rather than the interpreted
fraction, (b) look at GC/allocation pressure or I/O (the doc's own earlier
"something else entirely" guess, never investigated), or (c) accept that
closing the full ~60x gap to HotSpot (13.5s) requires safely compiling the
Lucene hot path at scale — a materially bigger undertaking (the postings
corruption bug LUCENE-POSTINGS.1 guards against) than anything tractable as
a "residual" of this doc.

## 2026-07-21 (later session) — host-degradation re-baseline + Lucene-ban re-test

**Host-degradation finding (not a code regression).** A fresh re-baseline this
session found `testSlicesDense` reproducibly failing with
`OutOfMemoryError: Java heap space` at both 2g and 4g heap (the doc's own
documented config), taking ~1800-2150s before OOMing — dramatically worse
than this doc's own 822.6-823.2s clean-completion figures from earlier the
same day. Bisection-by-elimination ruled out a code regression:

- Reverted the one plausible recent GC-adjacent commit
  (`ce4871079`, "perf(gc): make Arena::free_list_bytes() unconditionally
  O(1)") — OOM reproduced identically (2151s), refuting it.
- Built and ran the *exact* commit (`fa91ef9e5`) this doc's own 2026-07-21
  A/B/C measurement used to get 822.6s clean — it **also** OOM'd under
  today's host state (1745s, same failure signature). This conclusively
  shows the failure is host-environment-driven, not a `dev` regression:
  unchanged, previously-verified-clean code fails differently depending on
  the day's contention level on this shared box.
- Needed to raise the heap to 16g before a clean pass was achieved at all;
  even then the passing run took 3457s (baseline, ban intact) — roughly 4.2x
  the documented 822s. One run at 8g heap was killed by an external
  `SIGTERM` after ~50 minutes with zero progress logged (not this VM's own
  `--stack-dump-on-timeout` watchdog, not a JVM-reported OOM) — consistent
  with this being a heavily shared, actively-contended host today (load
  average climbed to 11.52 (15m) with 9 concurrent logged-in sessions during
  this investigation).

**Takeaway:** do not trust any single-run wall-clock number on this host as
a regression signal without also sanity-checking host load — this session's
numbers (1745-3457s across identical/near-identical code) show the swing can
be 2-4x on a single day, dwarfing the previously-documented ~37% variance.

**Lucene-ban-lift re-test — real throughput gain, but a real correctness
regression on broader coverage; ban must stay.** With both the java.util
narrowing (`0bdd62344`) and the MatchOps deopt-storm fix (`b416011bc`) now on
`dev` (neither was in place for this doc's original "lifting the ban doesn't
help" Finding 1), re-tested `CRATONVM_JIT_ALLOW_PACKAGES=org/apache/lucene/`
against current `dev` HEAD (`5aae2661e`), same seed, same host session (back
to back with the 16g baseline for a same-conditions comparison):

- `testSlicesDense`: passed (`OK (1 test)`), and **~26% faster** than the
  ban-intact baseline (2572s vs. 3457s, both at 16g heap, same run session).
  No exceptions, no corruption signature in the log.
- `ES812PostingsFormatTests` (the ban's own original correctness repro,
  32-test class, broader coverage) — **failed** with the ban lifted: an
  uncaught `java.lang.IllegalArgumentException: fromIndex(4) > toIndex(0)`
  in two `org.apache.lucene.tests.index.RandomPostingsTester$TestThread`
  worker threads, plus a
  `[cratonvm_vm::runtime::interpreter] implicit monitorexit on
  synchronized-method-frame-pop failed` warning (the `B8` diagnostic at
  `vm/src/runtime/interpreter.rs:6994-7015`) on
  `org/apache/lucene/index/IndexWriter.doWait` reporting
  `IllegalMonitorStateException: thread Thread-2 does not own the monitor`
  — before the run was externally killed (`Killed`, not this VM's own
  timeout) at ~1227s.
- **Control run, same class/seed, ban left in place (default Conservative
  policy, no env var): `OK (32 tests)`, clean, no monitor warning, no
  exception, completed in 1730s.** This isolates the failure to the ban
  being lifted — it is not a pre-existing/unrelated flake in this test class.

**Conclusion: the ban is still correctness-load-bearing, just not for the
originally-documented reason.** The 2026-07-19 re-investigation's belief that
"the ORIGINAL correctness justification for this ban is therefore probably
gone" (see the `LUCENE-POSTINGS.1` comment in `vm/src/jit/skip_list.rs`) is
superseded: there is a *different*, currently-live JIT correctness gap
around synchronized-method monitor handling (`IndexWriter.doWait`, which
implements a `synchronized` bounded-wait loop) that only surfaces once
Lucene is JIT-eligible, plus a downstream `RandomPostingsTester` failure
plausibly caused by the same underlying corruption. **Do not lift
`LUCENE-POSTINGS.1` based on the testSlicesDense speed win alone** — it does
not exercise the code path that breaks. Whoever picks this up next should
root-cause the synchronized-method-monitor interaction with JIT-compiled
`org/apache/lucene/index/IndexWriter` methods (starting from
`vm/src/runtime/interpreter.rs`'s `B8` monitor-frame-pop diagnostic and
whatever JIT-compiled call path reaches `IndexWriter.doWait`) before
attempting to lift the ban again — that fix, not another re-verification
pass, is the actual remaining blocker on the throughput side of this doc.

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

Expect `Time:` somewhere in the 600-825s range (varies by host day/load — see
the 2026-07-21 timing note above, don't treat a specific number as a tight
regression signal) and a suite-timeout-shaped JUnit failure (or `OK` if
`-Dtests.timeoutSuite` is raised past the actual completion time, currently
~825000 on this host). Use a `--stack-dump-on-timeout` value comfortably
above that if you want to rule out a real hang rather than just observing the expected slow completion.

## Resolution (2026-07-21)

The residual in the historical 2026-07-21 entry was traced to compiled
`ACC_SYNCHRONIZED` methods lacking the JVM implicit monitor contract. The
interpreter/JIT boundary now excludes synchronized methods from initial JIT
admission, invocation-counter upgrades, OSR, and both direct-callee compiler
paths. This is deliberately fail-closed until compiled monitor
prologue/epilogue support exists. The global Lucene package ban is therefore
removed, allowing the rest of the hot Lucene code to compile.

Validation on the Azure host (the final post-merge source was rebuilt into the uniquely named binary and re-ran the focused JIT/`--nojit` probe):

- Focused private synchronized-`wait()` probe: passed with JIT and with
  `--nojit` (`SYNC_JIT_PROBE_OK 2200` in both modes).
- `ES812PostingsFormatTests` with Lucene JIT enabled: `OK (32 tests)`,
  `Time: 1,439.677`; no monitor exception or postings corruption.
- Exact fixed-seed `testSlicesDense`, default configuration with no Lucene
  allow-list override on the feature build before an unrelated H2/Spring `dev` merge: `OK (1 test)`, `Time: 2,920.288`.

The dense time reflects a contended shared host and is not a strict
performance comparison point. The acceptance result is that the complete test
passes under the default, unbanned Lucene configuration without a timeout,
monitor failure, or data-corruption signature.
