# H2 suite sweep — running findings

## Baselines (RunOne per-class driver, -Xmx1g)
- HotSpot mem: 174 PASS / 16 SKIP / 7 FAIL / 2 CRASH / 1 HANG
- HotSpot disk: 186 PASS / 3 SKIP / 7 FAIL / 2 CRASH / 2 HANG
- HotSpot non-PASS (excluded from CratonVM bug attribution): TestFunctions, TestPersistentCommonTableExpressions,
  TestBnf, TestOutOfMemory, TestSubqueryPerformanceOnLazyExecutionMode, TestTools, TestMemoryUnmapper,
  TestPgServer, TestKeywords, TestMVStoreConcurrent, TestTriggersConstraints(disk HANG), TestCrashAPI(disk HANG), TestMVStore(disk FAIL)

## Root-cause clusters (CratonVM-specific)

### C1 — StringBuilder.repeat(int,int) ArrayStoreException  [FIX APPLIED in worktree]
`repeat` (JDK21+) not intercepted → real AbstractStringBuilder bytecode on synthetic char[] layout →
ensureCapacityNewCoder → Arrays.copyOf([B) over char[] → ArrayStoreException(src=Char,dest=Byte).
Used by DateTimeFormatter zero-padding. Affected (mem): TestListener, TestCompatibilityOracle,
TestCompatibilitySQLServer, TestLinkedTable, TestMultiThreadedKernel, TestSequence, TestSelectTableNotFound,
TestAlterTableNotFound, TestZloty, TestManyJdbcObjects, TestDatabaseEventListener, TestStringUtils ...
Fix: native_sb_repeat_codepoint in lang_string.rs + register (II)->SB/SBuffer.

### C2 — in-process javac "compiler message file broken"
H2 CREATE ALIAS/TRIGGER AS '<java>' uses javax.tools.JavaCompiler; CratonVM can't load compiler resource bundle.
Affected: TestView, TestFunctions, TestTriggersConstraints, TestCases(?), ...  (NOTE TestFunctions also fails on HotSpot mem)

### C3 — dropped FIRST character: BufferedReader.read shim breaks mark()/reset()  [FIX APPLIED]
ROOT CAUSE: phases_late.rs RWF86.1 registers UNGATED native shims for
BufferedReader.read([CII)I and read()I that delegate straight to the Reader at slot 0,
bypassing java.io.BufferedReader's buffer + mark/reset. BR_SIDETABLE (the only legit use)
is dead code (br_sidetable_register never called; Files.newBufferedReader builds a real BR),
so the shim fires for EVERY BufferedReader. H2 RUNSCRIPT/CSV BOM-skip does
`mark(1); read(); reset()` → reset() rewinds buffer indices the native read never advanced
→ first char lost. Confirmed: BufferedReader.mark/read/reset drops chars; reflection shows
read([CII) leaves nextChar/nChars=0. Both JIT and --nojit (not a JIT bug).
Affected: TestInit, TestRunscript, TestCsv, and any RUNSCRIPT/CSV/mark-reset path.
Fix: gate the two read shims behind #[cfg(feature="synthetic-jdk")] so real-JDK runs real bytecode.

### C6 — CRASHes (mem)
- TestReorderWrites: EXCEPTION_STACK_OVERFLOW (0xC00000FD) — deep/infinite recursion, stack shows ArrayList$Itr.hasNext/AbstractCollection. CratonVM-specific.
- TestCharsetCollator: IllegalArgumentException "Unsupported charset: cp500" (EBCDIC). CratonVM-specific.
- TestPgServer: shared w/ HotSpot (missing org.postgresql.jdbc.PgConnection) — EXCLUDE.
- TestKeywords: shared w/ HotSpot (clinit fail) — EXCLUDE.

### C4 — HANGs (CratonVM-specific, HotSpot passes fast) — need stack dumps
TestScript, TestCompatibility, TestFullText, TestIndex, TestTempTables, TestCancel, TestGetGeneratedKeys,
TestCachedQueryResults, TestNestedLoop, TestMvccMultiThreaded, TestMvccMultiThreaded2, TestAnalyzeTableTx, TestBtreeIndex ...

### C5 — misc NPE / correctness
TestPreparedStatement NPE; TestResultSet/TestConnection/TestTransaction/TestLobApi/TestSQLXML AssertionError.

## VERIFICATION (after repeat + BufferedReader fixes, new binary 01:27)
Targeted 15-class verify: 11 now PASS that were FAIL/HANG, incl. TestInit & TestCsv (BufferedReader),
TestListener/CompatibilityOracle/SQLServer/LinkedTable/Zloty/ManyJdbcObjects/DatabaseEventListener/
SelectTableNotFound/AlterTableNotFound/Sequence (repeat ArrayStore).
Remaining in verify set: TestStringUtils (now XML-content AssertionError, different bug),
TestSort (HANG->FAIL InvocationTargetException), TestRunscript (now reaches deeper, slow).

## HANGS RE-CHARACTERIZED: mostly PERF cliffs, not deadlocks
TestTempTables 707-sample dump: thread PROGRESSING through 10000-insert autocommit loop
(Insert.insertRows/MVPrimaryIndex.add/MVMap.operate/TransactionStore.commit/Page.clone), ~90x slower
than HotSpot -> exceeds timeout. The MVTable.lock frames are normal per-row locking, NOT a livelock.
=> Many silent hangs are MVStore insert/commit throughput cliffs. See bug-h2-mvstore-insert-loop-perf-hang.md.

## TIME vs HotSpot
73 classes PASS on both: HotSpot 326s vs CratonVM 1716s = 5.3x slower (fair speed metric).
