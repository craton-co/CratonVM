# `java.util.Properties` side-table's global 10,000-object cap silently drops writes for every `Properties` instance created afterward — process-lifetime, not concurrent-count

## Status
**OPEN** — new finding, 2026-07-21. Not H2-specific; a general
`java.util.Properties` correctness bug that H2's suite happens to trigger
because several test classes churn through thousands of JDBC connections
(each JDBC `Properties` object goes through this path).

## Severity
**HIGH** — silent data loss with no exception, in one of the most
fundamental JDK classes. Any sufficiently long-running process, or any
workload that creates many short-lived `Properties` objects (JDBC
connection properties being a common example), will eventually and
permanently lose the ability to store/retrieve properties on **new**
`Properties` instances, with `put`/`setProperty` becoming silent no-ops and
`getProperty` always returning `null`.

## Affected test classes
All fail with the generic, downstream H2 message
`org.h2.jdbc.JdbcSQLInvalidAuthorizationSpecException: Wrong user name or
password` (a *different* root cause from
`bug-h2-jaas-logincontext-two-arg-ctor-gap.md`'s identical-looking message —
see "How this differs from the JAAS finding" below):
- `org.h2.test.db.TestAnalyzeTableTx` — opens 10,000 connections to one DB
  in a loop, keeping each open.
- `org.h2.test.jdbcx.TestConnectionPool` (`testPerformance`) — 1,000
  pooled + 1,000 direct `DriverManager.getConnection` calls.
- `org.h2.test.jdbcx.TestDataSource` (`testDataSource`).
- `org.h2.test.synth.TestThreads` — many threads each opening connections.

All PASS on the HotSpot JDK25 baseline.

## Symptom
```
org.h2.jdbc.JdbcSQLInvalidAuthorizationSpecException: Wrong user name or password
	at org/h2/test/db/TestAnalyzeTableTx.test(TestAnalyzeTableTx.java:41)   # "Connection c = getConnection(...)" inside a loop
	at org/h2/engine/Engine.validateUserAndPassword(Engine.java:396)
```
Debug instrumentation added to `Engine.openSession` for this investigation
(temporary, reverted — not part of the fix) showed the exact moment of
failure:
```
[H2DEBUG] getUserName=SA foundUser=SA:2:org.h2.engine.User@2 ciHash=[...32 bytes...]
[H2DEBUG] validate=true
  ... (repeats correctly for ~4998 connections) ...
[H2DEBUG] getUserName= foundUser=null ciHash=[]
```
The `ConnectionInfo` for the failing connection has an **empty username and
an empty password hash** — not a hash mismatch. `DriverManager.getConnection(url,
user, password)` builds a `java.util.Properties` with `"user"`/`"password"`
keys internally; H2's `ConnectionInfo` reads them back via
`Properties.getProperty`. Both come back `null`/empty for a brand-new,
never-before-touched `Properties` object.

## Root cause
`native-builtins/src/properties_sidetable.rs` implements `java.util.Properties`
storage via a process-wide side-table (`Mutex<FxHashMap<usize,
FxHashMap<String,String>>>`, keyed by a GC-stable per-object identity) —
built originally to fix a *different* bug (KC26/Keycloak: synthetic
`Properties`' inner `Hashtable` field-shape didn't survive real bytecode
`put`/`get`). It enforces two caps, by design, to bound memory:
```rust
const MAX_PROPS_PER_OBJECT: usize = 10_000; // keys per single Properties object
const MAX_TOTAL_OBJECTS: usize = 10_000;    // distinct Properties objects, total
```
The insert path:
```rust
fn put_kv(ctx: &dyn NativeContext, obj: ObjectRef, key: &str, value: &str) {
    ...
    let k = key_for(ctx, obj);
    let mut t = table().lock();
    if t.len() >= MAX_TOTAL_OBJECTS && !t.contains_key(&k) {
        return;   // <-- silent no-op: no eviction, no exception, no logging
    }
    ...
}
```
**There is no eviction, LRU, weak-reference cleanup, or any mechanism that
ever removes an entry from the table** — not even when the underlying Java
`Properties` object becomes unreachable/is garbage collected. `MAX_TOTAL_OBJECTS`
is therefore not a "concurrently alive" cap as the comment describing it
implies ("Total-object cap ... Prevents accidental memory leaks from
short-lived Properties accumulating in the side-table") — it is a **cap on
the total number of `Properties` objects ever constructed in the process's
entire lifetime**. Once 10,000 distinct objects have ever been registered,
`put_kv` silently drops every write for any *new* object from then on, and
`get_kv` (which does a plain side-table miss → `None`) returns `null` for
it, exactly as if `getProperty` had never seen that key.

This is a straightforward reproduction with **no H2 involved at all**:
```java
for (int i = 0; i < 10001; i++) {
    Properties p = new Properties();
    p.put("user", "SA");
    p.put("password", "sa");
    if (p.getProperty("user") == null) { ... }   // first fails at i == 10000
}
```
confirmed under CratonVM (`cratonvm-h2-fail-triage-20260721`): the 10,001st
freshly-constructed `Properties` object gets `null` back for both keys,
immediately after `put`. Passes cleanly (10,001×) on the HotSpot JDK25
baseline.

For the H2 classes above, each `DriverManager.getConnection(url, user,
password)` call constructs at least one (in practice several — JDBC URL
parsing, `ConnectionInfo`'s own internal property bag, etc.) fresh
`Properties`-shaped object, so classes that legitimately open thousands of
connections in a session (`TestAnalyzeTableTx`'s 10,000-connection loop
being the most direct example) reliably cross the 10,000-object cap
part-way through and start getting silently empty connection properties for
every connection afterward — which H2 correctly (from its own point of view)
reports as "Wrong user name or password" since the username/password it read
really is empty.

## How this differs from the JAAS finding (`bug-h2-jaas-logincontext-two-arg-ctor-gap.md`)
Both surface as the identical H2 exception text, which is why they initially
looked like one cluster. They are unrelated:
- `TestAuthentication` fails on its **first** `AUTHREALM=...` connection
  (JAAS `LoginContext` 2-arg constructor gap; no connection-count threshold).
- `TestAnalyzeTableTx`/`TestConnectionPool`/`TestDataSource`/`TestThreads`
  fail only after crossing the 10,000-total-Properties-object watermark, via
  the plain (non-JAAS, non-`AUTHREALM`) username/password path in
  `Engine.openSession`.

Confirmed independent by heap size (256 MB vs. default 1 GB heap gave the
*exact same* failing connection index in the connection-loop repro — ruling
out a GC-timing correlation) and by the isolated `Properties`-only repro
above, which needs no JAAS, no H2, and no database connection at all.

## Fix direction
Give the side-table real lifecycle tracking instead of a hard total-count
ceiling — e.g. evict/reclaim entries once the underlying Java object is
collected (a `Weak`-keyed table, or hook `Properties` finalization/cleaner),
rather than treating "10,000 objects ever constructed" as "10,000 objects
alive now".

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.db.TestAnalyzeTableTx
```
or the standalone 10,001-iteration `Properties` loop above.
