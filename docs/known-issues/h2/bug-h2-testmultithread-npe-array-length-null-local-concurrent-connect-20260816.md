# `TestMultiThread.testViews` — `NullPointerException: Cannot read the array length because "<local4>" is null` during concurrent connection open

## Status
**OPEN, confirmed CratonVM-specific** — found 2026-08-16 on a GC-sweep rerun
(`origin/dev` @ `f80a4b775`, Azure host `azureuser@20.80.105.49`).
Differential-verified against real HotSpot JDK 25: **HotSpot PASSes in
6.6s**, same classpath, same H2 checkout.

## The failure
```
Exception in thread "main" java.util.concurrent.ExecutionException:
  org.h2.jdbc.JdbcSQLNonTransientException: General error:
  "java.lang.NullPointerException: Cannot read the array length because ""<local4>"" is null" [50000-249]
	at org.h2.test.db.TestMultiThread.main(TestMultiThread.java:57)
	at org.h2.test.db.TestMultiThread.test(TestMultiThread.java:66)
	at org.h2.test.db.TestMultiThread.testViews(TestMultiThread.java:270)
	at java.util.concurrent.FutureTask.get(FutureTask.java:193)
Caused by: org.h2.jdbc.JdbcSQLNonTransientException: General error: "java.lang.NullPointerException: ..."
	at java.util.concurrent.ThreadPoolExecutor$Worker.run(ThreadPoolExecutor.java:614)
	at org.h2.test.db.TestMultiThread.lambda$testViews$0(TestMultiThread.java:238)
	at org.h2.test.TestDb.getConnection(TestDb.java:31)
	at org.h2.test.TestDb.getConnectionInternal(TestDb.java:146)
	at java.sql.DriverManager.getConnection(DriverManager.java:199)
	at org.h2.Driver.connect(Driver.java:59)
	at org.h2.jdbc.JdbcConnection.<init>(JdbcConnection.java:137)
```
`testViews` (`TestMultiThread.java:238`, inside a lambda submitted to an
`ExecutorService`) opens its own JDBC connection per worker thread via
`TestDb.getConnection()` → `DriverManager.getConnection()` →
`org.h2.Driver.connect()` → `new JdbcConnection(...)`. Somewhere in that
connection-construction path, one worker thread hits a `NullPointerException`
reading the `.length` of an array-typed local variable that the JDK's
"helpful NPE" messages (JEP 358) can only name synthetically (`<local4>` —
no debug-symbol name survived, or none exists for it), which H2 then wraps
into a `JdbcSQLNonTransientException` and the test's `ExecutorService`
propagates as an `ExecutionException`.

## Why this looks CratonVM-specific, not a real race in H2 itself
* Confirmed absent on real HotSpot JDK 25 (`run-h2-suite.sh hotspot`), same
  binary classpath, same test — HotSpot passes cleanly.
* `testViews` deliberately opens N connections concurrently, one per thread,
  to the SAME database — exactly the shape that would surface a
  CratonVM-side thread-safety gap in whatever gets the null array: a shared
  (not per-connection) native array field being read by one thread before
  another thread has finished initializing it, or a lazily-allocated
  array-typed field that isn't safely published across threads under
  CratonVM's memory model but is under HotSpot's.

## Next steps
* Get a `--nojit` stack-dump-on-timeout style capture is not directly
  applicable here (this is a hard exception, not a hang) — instead, add a
  temporary diagnostic that prints the actual class/field of the null array
  read at the NPE site (CratonVM's own NPE-message-generation code should
  know which field access it evaluated even though the source local's debug
  name is unavailable) to identify the exact array field.
* Once the field is identified, check whether it is a `static`/shared field
  that should be per-`JdbcConnection`-instance, or a genuinely-shared field
  that needs a memory-visibility fix (volatile/synchronized init) for
  correct concurrent first-access under CratonVM specifically.
* Try to reduce to a smaller concurrent-connection-open repro (N threads,
  each doing `DriverManager.getConnection()` against one in-memory H2 DB)
  outside the full `TestMultiThread` class, to make this bisectable without
  the rest of the suite's noise.

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home <jdk25-home> --nojit --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.db.TestMultiThread
```
Not yet confirmed deterministic across repeated runs (concurrency-shaped
failures often aren't) — worth a few repeat runs before assuming it fires
every time.
