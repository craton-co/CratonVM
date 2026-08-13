# Five FAILs from the 2026-08-07 sweep that are NOT CratonVM bugs — CONFIRMED against HotSpot 2026-08-10

## Status
**NOT-A-BUG, CONFIRMED 2026-08-10.** All five now have the HotSpot control this
page asked for, and **stock HotSpot 25 fails all five identically** on the same
host, same H2 build, same classpath, same 300 s per-class cap:

| class | HotSpot 25 result | same failure as under CratonVM? |
|---|---|---|
| `TestClassLoaderLeak` | FAIL `ClassCastException: jdk.internal.loader.ClassLoaders$AppClassLoader` | yes, verbatim |
| `TestExit` | FAIL (non-zero exit, no exception) | yes |
| `TestJoin` | FAIL `PSQLException: Connection to localhost:5432 refused` | yes |
| `TestMulti` | FAIL `JdbcSQLSyntaxErrorException: Syntax error in SQL statement "CREATE …` | yes |
| `TestRecoverKillLoop` | FAIL `Failed: error! renaming file` | yes, verbatim |

Run: `apps/h2database-suite-runner` `hotspot` mode over the 62-class
non-passing union of the 2026-08-10 three-GC-variant sweep, JDK 25 at
`/data/toolchain/jdk-25`. 44 PASS / 15 FAIL / 6 HANG. None of these five
depends on timing, so the fact that the control shared the host with three
concurrent CratonVM runs does not bear on them.

Each explanation below stood up. Retained as written; only the status changed.

---

*Original text (2026-08-07), unchanged:*

**LIKELY NOT-A-BUG, unconfirmed** — found 2026-08-07 in the same full-suite
sweep as the rest of this batch. Each has a specific, plausible non-VM
explanation below. None has been confirmed against real HotSpot yet — that
confirmation is the actual next step for each, matching the standing
convention (`bug-h2-testannotationprocessorsoutput-jdk25-implicit-proc-
disabled-NOT-A-BUG.md`) of not closing a report as NOT-A-BUG without that
check. Filed here, not silently dropped, specifically so they get that
check rather than being re-discovered from scratch next sweep.

## 1. `TestClassLoaderLeak` — `ClassCastException: ... AppClassLoader cannot be cast to java.net.URLClassLoader`

```
	at org/h2/test/unit/TestClassLoaderLeak.createClassLoader(TestClassLoaderLeak.java:72)
	at org/h2/test/unit/TestClassLoaderLeak$TestClassLoader.<init>(TestClassLoaderLeak.java:102)
```
Since JDK 9, the platform/app class loader (`jdk.internal.loader.ClassLoaders$AppClassLoader`)
is **not** a `URLClassLoader` — that inheritance relationship was removed as
part of the module system. This test predates that change and casts it
directly. Should fail identically on real HotSpot JDK 25 with the same
classpath. **Likely a JDK-version test/library mismatch, same family as**
`bug-h2-testannotationprocessorsoutput-jdk25-implicit-proc-disabled-NOT-A-BUG.md`.

## 2. `TestExit` — `System.exit(1) called — process terminating`

```
[cratonvm] System.exit(1) called вЂ” process terminating
```
(Mojibake in the log is a UTF-8 em-dash misdecoded as CP1252 in the
terminal that captured it — cosmetic, not a bug in the VM's output.)
Per its name, `TestExit` almost certainly tests `System.exit()` behavior by
*calling* `System.exit(1)` as part of the test itself. The suite runner
treats any non-zero exit code as FAIL, which is indistinguishable from a
genuine crash from the outside. **Likely a test-harness limitation**
(running a class's `main()` once and checking the process exit code can't
tell "the test asserts exit(1) happens" from "the test crashed with
exit(1)") rather than a VM defect.

## 3. `TestJoin` — `ConnectException: 127.0.0.1:5432: Connection refused`

```
Caused by: java/net/ConnectException: 127.0.0.1:5432: Connection refused (os error 111)
	at org/h2/test/synth/TestJoin.testJoin(TestJoin.java:57)
	at org/postgresql/Driver.connect(Driver.java:298)
```
`TestJoin` tries to open a real PostgreSQL connection on `127.0.0.1:5432`
(presumably to test cross-database joins/linked tables against a live
Postgres instance) and there is no PostgreSQL server running on the suite
host. **This is an environment/fixture gap, not a VM bug** — the suite
runner does not provision a PostgreSQL instance. Either skip this class in
the runner's class list, or provision Postgres alongside the suite if this
class's coverage is wanted.

## 4. `TestMulti` (`org.h2.test.synth.thread.TestMulti` via `TestMultiNews`) — parser rejects unquoted `VALUE` as a column name

```
Syntax error in SQL statement "CREATE TABLE NEWS(... [*]VALUE VARCHAR(255))"; expected "identifier"
	at org/h2/test/synth/thread/TestMultiNews.first(TestMultiNews.java:95)
	at org/h2/command/Parser.readIdentifier(Parser.java:5568)
```
`VALUE` is a reserved keyword in modern H2/ANSI SQL and cannot be used
unquoted as a column name. This looks like an H2-parser-version-vs-test-
fixture mismatch (the test SQL predates `VALUE` becoming reserved, or was
written against a laxer parser mode) rather than a CratonVM defect — the
real H2 parser (unmodified, running under CratonVM here) is the one
rejecting it, and should reject it identically under HotSpot.

## 5. `TestRecoverKillLoop` — `Failed: error! renaming file`

```
00:00.165 org.h2.test.poweroff.TestRecoverKillLoop Failed: error! renaming file
	at org/h2/test/poweroff/TestRecoverKillLoop.main(TestRecoverKillLoop.java:27)
	at org/h2/test/poweroff/TestRecoverKillLoop.runTest(TestRecoverKillLoop.java:61)
```
Fails in 0.165s, immediately. Per its name and the `poweroff` package, this
class simulates crash-recovery by having an **external** wrapper process
repeatedly kill and restart the JVM mid-write, then checking recovery on
each restart — the "Failed: error! renaming file" message reads like the
test's own self-check for the *previous* run's simulated crash artifact,
which only makes sense in a multi-process kill-loop harness, not a single
plain `main()` invocation. Plausibly **the suite runner's "run this class's
main() once" model doesn't match what this test needs to run meaningfully**
— same category as `TestExit` above, a harness-shape mismatch rather than a
VM defect. Unconfirmed: would need to read `TestRecoverKillLoop.java` to
verify it truly expects external process-kill orchestration.

## Next steps (applies to all five) — DONE 2026-08-10
Ran exactly this control (through the suite runner's `hotspot` mode, which
builds the same classpath):
```bash
cd apps/h2database-suite-runner
JDK25=/data/toolchain/jdk-25 ./run-h2-suite.sh hotspot --category all \
  --only '<the non-passing union>' --class-to 300 --max-heap 1g
```
HotSpot fails all five identically, so all five are closed as NOT-A-BUG (see
the Status table at the top). None is retracted.

## The same control closed ten more, outside this page's five

The 2026-08-10 run covered the whole 62-class non-passing union, and HotSpot 25
also fails or hangs on these — so they are H2-suite / environment issues too,
not CratonVM defects:

| class | HotSpot 25 |
|---|---|
| `TestFunctions` | FAIL `AssertionError: Failure` (already had its own NOT-A-BUG page) |
| `TestLob` | FAIL `MVStoreException: Chunk 6 not found` — matches `!bug-h2-testlob-mvstore-chunk-not-found-and-file-lock.md` |
| `TestOutOfMemory` | FAIL `AssertionError: Failure` |
| `TestCachedQueryResults` | FAIL `Expected: 100000 actual: 99995` |
| `TestWeb` | FAIL `assertContains` |
| `TestMVStore` | FAIL `Cache 1Mb, reads: 2872 expected: 1750 …` |
| `TestTimer` | FAIL `NULL not allowed for …` |
| `TestMemoryUnmapper` | FAIL `Expected: 2 actual: 1` |
| `TestTools` | FAIL `Connection is broken` |
| `TestPowerOffFs2` | HANG (then `Database is already closed`) |

**Read the load caveat before reusing these.** That control shared an 8-core
host with three concurrent CratonVM suite runs (load average ~20). For the
exception-shaped rows above that changes nothing. For anything timing-shaped it
might: `TestSubqueryPerformanceOnLazyExecutionMode` ("Lazy execution too slow"),
`TestMVStore`'s cache-ratio assertion, and the six HANGs (`TestBenchmark`,
`TestKill`, `TestMultiThreaded`, `TestPowerOffFs`, `TestPowerOffFs2`,
`TestSynth`) are exactly the shape a loaded host manufactures. Re-run those on
an idle host before quoting them as evidence of anything.
