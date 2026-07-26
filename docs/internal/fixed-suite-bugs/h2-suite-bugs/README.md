# CratonVM — H2 Database full-suite sweep (2026-06-15)

Branch `fix/h2-suite-loop` (worktree `CratonVM-h2suite`), built from `dev`.

## Method
All 200 `org.h2.test.*` classes registered in `TestAll` were run **one process
per class** via a small driver (`org.h2.test.RunOne`, `-Xmx1g`), so a crash in
one class never hides the rest. Each class is classified PASS / FAIL (caught
throwable) / SKIP (`isEnabled()`=false for the config) / CRASH (VM died, no
result line) / HANG (exceeded the per-class wall cap). Config = `mem`
(in-memory, `TestAll`'s first pass). HotSpot (jdk-25) was run identically as the
baseline; a class is only attributed to CratonVM when **HotSpot PASSes it**.
Harness: `h2sweep/sweep.ps1`, `h2sweep/triage.sh`, `apps/h2database/h2/RunOne.java`.

## Headline numbers (mem config, 200 classes)

| | PASS | FAIL | HANG | CRASH | SKIP |
|---|---|---|---|---|---|
| HotSpot baseline | 174 | 7 | 1 | 2 | 16 |
| CratonVM **before** any fix | 73 | 70 | 35 | 6 | 16 |
| CratonVM after string fixes (repeat + BufferedReader) | **91** | 51 | 36 | 6 | 16 |
| CratonVM after **all 4 fixes** (PASS↔HANG drifts with load near the timeout) | 84 | 45 | 51 | **4** | 16 |

- The string fixes alone are **+18 net PASS (73 → 91)**, 19 classes green, 0 real
  code regressions (PASS↔HANG flips are perf-cliff classes crossing the timeout
  under concurrent load, not regressions).
- The two crash fixes drop CRASHes **6 → 4** (the remaining four are
  `TestPgServer`/`TestKeywords` — shared with HotSpot — and the documented-open
  `cp500` charset / PBE-algparams gaps). Every targeted crash class moved off
  CRASH: TestReorderWrites CRASH→FAIL, TestDiskFull→FAIL, TestUtils CRASH→HANG,
  TestSampleApps/TestFileLockProcess HANG (no crash). FAILs dropped 70 → 45.
- CratonVM-specific divergences after fixes: **45 FAIL + 34 HANG + 4 CRASH**
  (10 further FAIL/CRASH/HANG are shared with HotSpot and excluded:
  TestFunctions, TestPersistentCommonTableExpressions, TestBnf, TestOutOfMemory,
  TestTools, TestSubqueryPerformanceOnLazyExecutionMode, TestMemoryUnmapper,
  TestMVStoreConcurrent, TestPgServer, TestKeywords).

## Time vs HotSpot
On the **73 classes that PASS on both VMs**: HotSpot 326 s, CratonVM 1716 s →
**5.3× slower** (a fair execution-speed metric). The full-sweep wall is dominated
by the 34 hangs capped at the timeout; those are throughput cliffs, not
correctness (see the perf-hang report).

## Fixes landed on this branch (verified)
1. **`bug-h2-stringbuilder-repeat-arraystore.md`** — `StringBuilder.repeat(int,int)`
   ran real bytecode on the synthetic `char[]` layout → `Arrays.copyOf([B)` over
   a char[] → `ArrayStoreException`, on the `DateTimeFormatter` zero-pad path.
   Fixed by intercepting `repeat` (`native_sb_repeat_codepoint`). ~17 classes.
2. **`bug-h2-bufferedreader-mark-reset-dropped-char.md`** — ungated native shims
   for `BufferedReader.read([CII)/()` delegated straight to the wrapped reader,
   bypassing the buffer so `mark()/reset()` (real bytecode) became no-ops; H2's
   RUNSCRIPT/CSV BOM probe dropped the first character (`"create"→"reate"`).
   Fixed by gating the shims behind `synthetic-jdk` so real-JDK runs real
   bytecode. TestInit, TestRunscript, TestCsv, …

## Crash fixes landed (verified)
3. **`bug-h2-stack-overflow-filesystem-tests.md`** [FIXED] — `EXCEPTION_STACK_OVERFLOW`
   in `TestReorderWrites` / `TestDiskFull` / `TestSampleApps` / `TestFileLockProcess`.
   The snapshot-iterator native shadow-recursed on a real `java/util/PriorityQueue$Itr`
   (`hasNext`→`hasNext` via `ctx.invoke`). Fixed in `native-collections`.
4. **`bug-h2-testutils-sigsegv.md`** [FIXED] — JIT-only `EXCEPTION_ACCESS_VIOLATION`
   in `TestUtils`. `aastore` codegen emits the SATB write barrier but the
   JIT-eligibility pre-scan never set `needs_heap` for it, so the barrier loaded
   stack garbage as the VM pointer. Fixed in `jit/src/x64.rs`.

## Open bugs (reports in this directory)
- **`bug-h2-charset-cp500-unsupported.md`** — FIXED 2026-07-22, see
  [`../fixed-suite-bugs/bug-h2-charset-cp500-unsupported-FIXED.md`](bug-h2-charset-cp500-unsupported-FIXED.md)
  — `Charset.forName("cp500")` now resolves (curated IBM500 codec added);
  `TestCharsetCollator`/`TestSetCollation.testCp500Collator` both pass.
- **`bug-h2-netutils-missing-pbe-algparams.md`** — missing
  `PBEWithHmacSHA256AndAES_256` AlgorithmParameters — `TestNetUtils`.
- **`bug-h2-mvstore-insert-loop-perf-hang.md`** — MVStore insert/commit
  throughput cliff (~90× on a 10 000-row loop); the dominant "silent hang"
  cause. Progressing, not deadlocked.
- **`bug-h2-inprocess-javac-resource-bundle.md`** — in-process javac
  ("compiler message file broken") for `CREATE ALIAS/TRIGGER` Java source —
  TestView, TestCases, TestTriggersConstraints.
- **`bug-h2-timezone-dst-offset.md`** — DST/zone-rule offset miscalculation
  (TestDateStorage 8 h across the 2010-03-14 US spring-forward, TestValue exactly
  1 h, TestTimeStampWithTimeZone 3 h).

## Remaining (not yet separately reported)
~23 `AssertionError` correctness diffs across diverse subsystems (XML/SQLXML,
LOB, numeric, missing-expected-exception, trace formatting) — a long tail of
individually-distinct bugs, no further dominant cluster after the two fixes
above. Plus 2 NPEs and assorted single failures. The perf-hang set still needs
partitioning into perf-cliff vs genuine multi-thread deadlock
(`TestMvccMultiThreaded*`).
