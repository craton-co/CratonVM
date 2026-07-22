# `TestConnectionPool`/`TestDataSource` — "Wrong user name or password" is NOT the properties-sidetable cap bug (root cause still open)

## Status
**OPEN** — new finding, 2026-07-22, split out of
`bug-h2-properties-sidetable-global-cap-silent-drop.md` (now FIXED — see
`docs/internal/h2-suite-bugs/bug-h2-properties-sidetable-global-cap-silent-drop-FIXED.md`)
after that fix was verified not to change either class's outcome.

## Severity
**HIGH** — same downstream symptom as the cap bug (silent/incorrect
credential loss on a fresh JDBC connection), but with a distinct, still
unidentified root cause. Confirmed CratonVM-specific (HotSpot JDK25 passes
both classes cleanly).

## Affected test classes
- `org.h2.test.jdbcx.TestConnectionPool` (`testPerformance`)
- `org.h2.test.jdbcx.TestDataSource` (`testDataSource`)

Both fail with:
```
org.h2.jdbc.JdbcSQLInvalidAuthorizationSpecException: Wrong user name or password
	at org/h2/test/jdbcx/TestConnectionPool.testPerformance(TestConnectionPool.java:171)
	at java/sql/DriverManager.getConnection(DriverManager.java:199)
	at java/sql/DriverManager.getConnection(DriverManager.java:613)
	at org/h2/Driver.connect(Driver.java:59)
	at org/h2/jdbc/JdbcConnection.<init>(JdbcConnection.java:124)
	at org/h2/engine/SessionRemote.connectEmbeddedOrServer(SessionRemote.java:350)
	at org/h2/engine/Engine.createSession(Engine.java:206)
	at org/h2/engine/Engine.validateUserAndPassword(Engine.java:396)
```
(line 171 is the `DriverManager.getConnection(url, user, password)` call
inside `testPerformance`'s "direct" 1000-connection loop.)

## Why this is a different bug from the (now-fixed) properties-sidetable cap
Both `TestConnectionPool`/`TestDataSource` were originally lumped in with
`TestAnalyzeTableTx`/`TestThreads` purely because they share the identical
downstream H2 exception text — exactly the trap the original cap doc's own
"How this differs from the JAAS finding" section warned about (three
unrelated bugs now share this one exception message). Evidence they are
**not** the cap bug:
- The properties-sidetable's `MAX_TOTAL_OBJECTS` (10,000) cap is never even
  approached during either class's run — confirmed via temporary
  instrumentation (`table().lock().len()` stayed in the low hundreds the
  entire time).
- The cap fix (weak-reference-based reclaim, see the FIXED doc) changes
  nothing about either class's outcome — both fail identically before and
  after.
- `--nojit` (interpreter-only execution) doesn't change the outcome either —
  rules out a JIT-tier-up-specific dispatch bug.
- The unfixed baseline binary (pre-dating this investigation) fails at the
  exact same point.

## What's actually observed (evidence gathered, root cause NOT pinned down)
With temporary `CRATONVM_DIAG_PROPERTIES=1` instrumentation added to
`native-builtins/src/properties_sidetable.rs`'s `put_kv` (since removed —
not part of the fix):
- `DriverManager.getConnection(url, user, password)`'s real bytecode
  (confirmed via `javap -c java.sql.DriverManager`) does exactly
  `new Properties(); info.put("user", user); info.put("password", password);`
  — both `put()` calls **do** reach the side-table's native override and
  **do** succeed (`is_new=...` observed both ways depending on which
  connection in the loop), immediately followed by H2's own
  `ConnectionInfo` copying those into its *own* internal, uppercase-keyed
  properties bag (`"USER"`, `"PASSWORD"`, plus `MV_STORE`/
  `MAX_COMPACT_TIME`/`LOCK_TIMEOUT` defaults) — those puts also succeed.
- Despite both puts succeeding immediately beforehand, the very next thing
  that happens is `Engine.validateUserAndPassword` throwing "wrong user
  name or password" for that connection.
- This is a different failure shape than the (now-fixed) cap bug, which was
  a pure registration-time reject (put silently refused, so a later
  `getProperty` finds nothing because nothing was ever stored). Here, the
  write appears to succeed and the very next read still fails — suggesting
  either (a) `Engine.validateUserAndPassword` reads from a *different*
  object than the one that was just populated (an object-identity mismatch
  between the JDBC-supplied `Properties` and whatever `ConnectionInfo`
  actually consults), or (b) a race/ordering issue specific to this
  call-stack depth (`DriverManager` → `Driver.connect` → `JdbcConnection`
  → `SessionRemote` → `Engine`) that a flat, shallow test loop (like the
  cap bug's own standalone repro) doesn't reproduce.
- Not yet checked: whether `JdbcConnectionPool`/`JdbcDataSource`'s own
  connection-acquisition path (as opposed to the plain
  `DriverManager.getConnection(url,user,password)` 3-arg overload used
  directly in `TestConnectionPool.testPerformance`'s second loop) is
  involved, since the pooled loop earlier in the same test method succeeds.

## Suggested next steps for whoever picks this up
1. Re-add the `CRATONVM_DIAG_PROPERTIES=1` instrumentation (or equivalent)
   to trace `Engine.validateUserAndPassword`'s actual read path —
   specifically, what object identity it reads `USER`/`PASSWORD` from
   relative to the object `ConnectionInfo`'s constructor wrote them to.
2. Try a minimal standalone repro that calls
   `DriverManager.getConnection(url, "sa", "sa")` directly (bypassing
   `JdbcConnectionPool`/`TestBase` entirely) against a plain embedded H2 URL,
   to isolate whether the pooled-connection setup earlier in the test is a
   precondition or incidental.
3. Check for object-identity confusion between the `Properties` object
   `DriverManager` builds and the one `ConnectionInfo`/`SessionRemote`
   actually reads from — this general class of bug (GC-stable-identity
   aliasing) has precedent in this exact file (see the "GC-stable side-table
   key" section of `properties_sidetable.rs` and the Hibernate password-mask
   bleed bug it fixed).

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home <jdk25> \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.jdbcx.TestConnectionPool
```
Fails deterministically inside `testPerformance`'s direct-connection loop.
Passes cleanly under real HotSpot JDK25 (`$JDK25/bin/java` with the same
classpath).
