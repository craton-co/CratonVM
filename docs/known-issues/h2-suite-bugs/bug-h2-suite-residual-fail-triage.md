# H2 suite — residual FAIL triage (2026-07-21): reproduced, narrowed, not fully root-caused

## Status
**OPEN, mixed confidence** — this doc collects the remaining genuine
CratonVM-vs-HotSpot FAILs from the 2026-07-21 full-218-class rerun that each
got a repro and an initial hypothesis, but not a full root-cause
investigation, within this session's time budget. Each is independent;
none share a confirmed root cause with each other or with the other,
fully-written-up docs in this directory (though several are plausible
candidates for future consolidation once investigated further — noted
per-item below). Follow the "next step" for whichever item is picked up.

All classes below PASS on the HotSpot JDK25 baseline; all FAIL under
CratonVM `jit-real` (real JDK25 backend).

## `org.h2.test.jdbc.TestPreparedStatement` (`testDate8`) — calendar-system date discrepancy
```
AssertionError: Expected: 1582-09-15 00:00:00.000 actual: 1582-09-24 23:00:00.000
```
A ~9-day-23-hour delta on a 1582 date (the year the real Gregorian calendar
was adopted). Not a timezone-offset shift (see
`bug-h2-timezone-zonerules-offset-miscalculation.md` for that, separate,
class of bug) — the magnitude and the specific year strongly suggest a
Julian-vs-proleptic-Gregorian calendar-system mismatch somewhere in the
`java.time`⟷`java.sql.Date`/`Timestamp` bridging path (`java.time` is always
proleptic Gregorian; legacy `java.util.GregorianCalendar` has a real
historical Julian→Gregorian cutover at 1582-10-15 by default). **Next
step:** isolate with a direct `LocalDate.of(1582,9,15)` → `java.sql.Date`
→ back round-trip, no H2, and compare against HotSpot.

## `org.h2.test.unit.TestFileLock` (`testSimple`) — wrong exception/error-code on concurrent DB open
```
AssertionError: Expected an SQLException or DbException with error code 90020, but got a org.h2.message....
Caused by: JdbcSQLNonTransientConnectionException: Error opening database: "Concurrent update"
```
The test deliberately races two connections against the same DB file and
`assertThrows` a *specific* H2 error code (90020,
`DATABASE_ALREADY_OPEN_1`-family). CratonVM's file-locking/DB-open race
produces a different H2-level exception ("Concurrent update", H2's
optimistic-locking-conflict code) instead. Plausibly a genuine timing
difference in CratonVM's `FileChannel`/advisory-lock semantics changing
which of H2's two internal race-detection paths fires first — not
investigated beyond the repro. **Next step:** compare H2's `FileLock.lock()`
lock-acquisition timing/retry behavior under CratonVM vs. HotSpot directly.

## `org.h2.test.store.TestRandomMapOps` (`seed:0 op:9`) — MVMap reverse-view assertion
```
AssertionError: rev (1654, null)
```
Deterministic given the test's fixed `seed:0` (a `Random`-driven fuzzer over
`MVMap` operations, comparing forward vs. reverse-iteration views). Given
this session separately found a real `TreeMap.tailMap()`/submap-view
corruption bug (`bug-h2-treemap-tailmap-headmap-view-corruption.md`), and
H2's `MVMap`/`TransactionMap` machinery leans on `NavigableMap`-style
range/reverse views internally, this is a **plausible manifestation of that
same bug** rather than an independent one — but not confirmed. **Next
step:** rerun with `seed:0` standalone (no full suite) and check whether the
specific op sequence touches a submap/`tailMap`/`headMap` view at op #9.

## `org.h2.test.synth.TestFuzzOptimizations` — NullPointerException deep in query optimizer recursion
```
NullPointerException: ConditionAndOr.right
  at org/h2/expression/condition/ConditionInQuery.getValue (deeply recursive, many repeated frames)
```
A randomized SQL-fuzzing test; the NPE is on a `ConditionAndOr` node's
`right` operand being null partway through a recursive `IN (SELECT ...)`
evaluation. Could be a genuine CratonVM interpreter/GC bug corrupting a
field read partway through deep recursion, or a real (rare) H2 optimizer bug
that a fuzzer with a different random seed / scheduling would also hit on
HotSpot eventually — not distinguished in this session. **Next step:** check
whether the fuzzer's seed is fixed (deterministic repro) or wall-clock-seeded
(need many HotSpot runs to rule out "just never hit it there").

## `org.h2.test.db.TestLinkedTable` (`testHiddenSQL`) — password redaction / linked-table SQL text mismatch
```
AssertionError: ... Table "DUAL2(...)" not found ... does not contain: pwd
```
Tests that a `LINKED TABLE` failure's exception message doesn't leak the
linked connection's password, using a table name (`DUAL2`) that doesn't
exist to force a specific error path. The actual H2 exception text differs
from what the test expects to find/not-find. Not narrowed further — could
be a message-formatting difference or a genuinely different failure path
inside `LinkedTable`'s connect-and-validate step under CratonVM.

## `org.h2.test.unit.TestUpgrade` — `SecurityException` unloading the H2 driver
```
InvocationTargetException -> SecurityException
  at org/h2/tools/Upgrade.unloadH2 -> org/h2/Driver.unload -> java/sql/DriverManager.deregisterDriver
```
Real `DriverManager.deregisterDriver()` performs a caller-classloader check
(`isDriverAllowed(driver, callerClass)`, using `Reflection.getCallerClass()`-style
caller detection) and throws `SecurityException` if the caller's classloader
doesn't match/dominate the driver's. A caller-class misdetection would be
consistent with the `StackWalker`/caller-sensitive-method native gaps this
codebase has hit before in other suites (see `docs/known-issues/README.md`'s
`T19.H2: StackWalker boot-time natives` entries) — plausible but not
confirmed here. **Next step:** print `Reflection.getCallerClass()` /
`StackWalker.getInstance(RETAIN_CLASS_REFERENCE).getCallerClass()` from
inside `Driver.unload()`'s call chain and compare to HotSpot.

## `org.h2.test.unit.TestShell` — NPE reading Shell tool output
```
NullPointerException: Cannot invoke "String.startsWith(String)" because "text" is null
  at org/h2/test/unit/TestShell.read -> TestBase.assertStartsWith
```
The test drives H2's interactive `Shell` tool via piped stdin/stdout and
reads a line of its output; that line comes back `null` where a real prompt
string was expected. Superficially similar in shape (a `null` string where
one shouldn't be) to the `TreeMap` submap corruption in
`bug-h2-treemap-tailmap-headmap-view-corruption.md`, but this is a
completely different code path (interactive process I/O, not a collection
view) — **not** merged with that doc; flagged as its own gap in
CratonVM's piped-process stdin/stdout handling for the `Shell` tool
specifically. Not investigated further.

## `org.h2.test.db.TestAlter` (`testAlterTableDropIdentityColumn`) — `AssertionError: Expected: 1 actual: 0`
Not investigated beyond the repro — a boolean/count-style assertion in an
`ALTER TABLE ... DROP COLUMN` (identity column) test.

## `org.h2.test.db.TestTransaction` (`testMergeUsing`) — `AssertionError: Expected: 100 actual: 50`
Not investigated beyond the repro — exactly half the expected row count
survived a `MERGE USING` transaction-isolation test; worth checking whether
this is a concurrency/visibility bug (half the rows from one of two
concurrent transactions) given the "exactly half" shape.

## `org.h2.test.unit.TestBnf` (`testProcedures`) — `AssertionError: Expected: true got: false`
Not investigated beyond the repro — a BNF-grammar-driven autocomplete/procedure
test.

## `org.h2.test.store.TestDataUtils` (`testParse`) — `AssertionError: Expected: 1 actual: 1`
Both sides print as `"1"` yet `assertEquals` failed — this loop
(`for (long i = -1; i != 0; i >>>= 1) { ... }`) exercises `DataUtils.parseHexLong`/
`parseHexInt` round-trips at every power-of-two bit-shift boundary of a
64-bit value; the identical-looking printed values suggest either a
type-mismatch in the comparison (e.g. `long` vs. boxed `Integer`/`Long`
which would print the same digit but fail `.equals()`) or a real numeric
difference that happens to coincide in decimal display at whichever
boundary iteration failed. Not narrowed to which iteration/value. **Next
step:** instrument the loop to print `i` and both operands' `getClass()`
each iteration.

## `org.h2.test.poweroff.TestRecoverKillLoop` — excluded as non-comparable, not a new CratonVM FAIL
CratonVM fails at `00:00:00.000` (first iteration); the HotSpot baseline
"fails" too, but only after **5 hours 16 minutes** of continuous
kill/restart-loop stress testing, with the identical generic failure text
("`error! renaming file`" — the test's only failure message for any rename
error, from any cause). This class's `main()` is one of the H2 suite's rare
exceptions to the standard `TestBase.createCaller().init().testFromMain()`
convention (see `run-h2-suite.md`'s "217 of 218 classes" caveat): its
`main()` directly calls `runTest(Integer.MAX_VALUE)` — an intentionally
open-ended, manual stress-test entry point (repeatedly spawning and
`kill -9`-ing a child `TestRecover` JVM process), not something meant to
"pass" or "fail" cleanly inside a per-class-timeout harness. Given both VMs
eventually hit a failure with the same generic message and this test isn't
designed to terminate cleanly either way, this is **not counted as a
genuine new CratonVM regression** — but the fact that CratonVM's *first*
child-process iteration already fails (vs. HotSpot's ~1900+ successful
kill/restart cycles before its eventual failure) does suggest something
about CratonVM's `Runtime.exec()`-spawned child-process handling for this
specific test is worth a dedicated look if this class matters going
forward. Not investigated further this session.

## Repro (representative — swap the class name)
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.db.TestAlter
```
