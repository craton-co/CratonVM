# Five further FAILs from the 2026-08-07 full-suite sweep: `TestPgServer`, `TestTools`, `TestMemoryUnmapper`, `TestFileLock`, `TestTimer`

## Status
**OPEN, single-sample each** — found 2026-08-07 in a full 218-class suite
sweep on a clean host (`origin/dev` merge @ `f9315411a`, load average ~14).
Grouped here for efficiency of triage; none has been root-caused yet, and
they are not asserted to share a cause with each other.

## 1. `TestPgServer.testDateTime` — NPE in the pgjdbc driver, not H2 itself

```
Exception in thread "main" java/lang/NullPointerException: Cannot invoke "String.getBytes(java.nio.charset.Charset)" because "statementName" is null
	at org/h2/test/unit/TestPgServer.main(TestPgServer.java:51)
	at org/h2/test/unit/TestPgServer.testDateTime(TestPgServer.java:630)
	at org/postgresql/jdbc/PgPreparedStatement.execute(PgPreparedStatement.java:180)
	...
	at org/postgresql/core/v3/QueryExecutorImpl.sendCloseStatement(QueryExecutorImpl.java:1907)
```
The NPE is inside the real **pgjdbc** driver (`org.postgresql...`), talking
to H2's PG-wire-protocol server emulation over a real socket. `statementName`
null on `sendCloseStatement` suggests a prepared-statement's server-side
name was never assigned/returned during `sendParse`, and pgjdbc's own
close-statement cleanup then NPEs on it. Could be H2's `PgServer` sending a
malformed or incomplete `Parse` response over the wire, or a timing/ordering
issue specific to how CratonVM schedules the PgServer thread vs the test's
own connection thread. Not yet distinguished from an H2-level protocol gap.
Prior rounds saw this class fail at 650-850s with the same statement wedged
mid-protocol; this round failed fast (before hitting that point), which is
itself worth noting — the failure mode may not be deterministic run to run.

**Next step**: capture the raw PG-wire bytes H2's `PgServer` sends for the
`Parse`/`ParseComplete` exchange around `testDateTime` and diff against what
pgjdbc expects, or against the same exchange captured against real HotSpot.

## 2. `TestTools.testSSL → runServer → testServerMain` — `Expected: 0 actual: 1`

```
06:44:36 08:10.256 org.h2.test.unit.TestTools Expected: 0 actual: 1
	at org/h2/test/unit/TestTools.testServerMain(TestTools.java:613)
	at org/h2/test/unit/TestTools.testSSL(TestTools.java:650)
	at org/h2/test/unit/TestTools.runServer(TestTools.java:708)
```
~8 minutes of real work happened before the assertion (490s+), so this is
not an immediate/config-level failure — an SSL-enabled H2 TCP server was
started and exercised first. The `0` vs `1` shape (likely an exit code or
error count from a spawned server process) needs `TestTools.java:613`
context to interpret. Possibly related to the general slow-server-startup
symptoms seen elsewhere in this sweep (`TestWeb`, `TestPgServer`) if the
counted "1" is a late-arriving/leftover process, or a genuine SSL-handshake
defect under CratonVM.

## 3. `TestMemoryUnmapper` — `Expected: 2 actual: 1`

```
WARNING: sun.misc.Unsafe::invokeCleaner has been called by org.h2.util.MemoryUnmapper
00:00.000 org.h2.test.unit.TestMemoryUnmapper Expected: 2 actual: 1
	at org/h2/test/unit/TestMemoryUnmapper.test(TestMemoryUnmapper.java:53)
```
Instant failure (0.000s) — this is a synchronous count mismatch, not a
timing issue. `MemoryUnmapper` wraps `sun.misc.Unsafe::invokeCleaner` to
force-unmap a `MappedByteBuffer`; "expected 2, actual 1" suggests the test
unmaps in two different ways (or checks two distinct buffers/paths) and
one of them isn't registering as unmapped. Thematically adjacent to
the retired `bug-h2-niomapped-unmap-gc-timeout` write-up (a *different* class,
`TestFileSystem`'s `nioMapped:` unmap timeout) but not confirmed to share a
cause — and that one is now FIXED, and was not what it looked like: not
GC-timing or weak-reference clearing, but the conservative JIT root scan marking
the whole native stack on the leftovers of a compiled frame that had already
returned. This one is an immediate count check with no timeout involved at all,
so the fix there is not expected to touch it.

## 4. `TestFileLock.testSimple` (inside `assertThrows`)

```
Caused by: org/h2/jdbc/JdbcSQLNonTransientConnectionException: Error opening database: "Concurrent update"
	at org/h2/test/unit/TestFileLock.lambda$testSimple$0(TestFileLock.java:99)
	at org/h2/store/FileLock.lock(FileLock.java:110)
	at org/h2/store/FileLock.lockFile(FileLock.java:337)
	at org/h2/store/FileLock.getExceptionFatal(FileLock.java:429)
	at org/h2/test/TestBase.assertThrows(TestBase.java:1653)
```
This trace is captured from *inside* `TestBase.assertThrows` — the test
expects **some** exception from the lambda, and the exception shown
(`"Concurrent update"`) may be exactly what's expected, or may be the wrong
exception type/message vs what `assertThrows` was told to check for. The
0.5s duration (near-instant) is consistent with a lock-contention simulation
rather than a hang. Needs `TestFileLock.java:99`'s actual `assertThrows`
argument (expected exception class) to tell whether this is a real mismatch
or whether the extracted log window simply doesn't show the actual
`AssertionError` (if any) that followed.

## 5. `TestTimer.loop` — `NULL not allowed for column "ID"`

```
Exception in thread "main" org/h2/jdbc/JdbcSQLIntegrityConstraintViolationException: NULL not allowed for column "ID"
	at org/h2/test/synth/TestTimer.loop(TestTimer.java:60)
	at org/h2/jdbc/JdbcStatement.execute(JdbcStatement.java:231)
	at org/h2/table/Table.convertInsertRow(Table.java:947)
	at org/h2/table/Column.validateConvertUpdateSequence(Column.java:406)
```
An INSERT is producing `NULL` for a column H2 itself flags as NOT NULL,
inside `Column.validateConvertUpdateSequence` — this is H2's own
auto-generated-value (sequence/identity) machinery failing to produce a
value for the `ID` column on this particular insert. If `ID` is an identity
column, this suggests CratonVM-side identity/sequence value generation is
returning null under whatever timing `TestTimer.loop` exercises (per the
class name, likely a timer-driven repeated-insert scenario) — worth checking
whether this is a race between sequence allocation and the row-validate
step.

## Repro (representative, adjust class name)
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 --nojit --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.unit.TestPgServer   # or TestTools / TestMemoryUnmapper / TestFileLock / org.h2.test.synth.TestTimer
```
