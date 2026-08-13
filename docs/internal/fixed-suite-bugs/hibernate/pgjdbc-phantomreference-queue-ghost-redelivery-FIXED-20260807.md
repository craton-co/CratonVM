# pgjdbc `parsedQueryMap.remove(ref)` returns null — `ReferenceQueue` delivers an already-processed `PhantomReference` a second time — FIXED

**Status:** FIXED 2026-08-07. Found during the real-Postgres full-suite Hibernate
run (`pgsql-fullsuite-20260807`). This was the single dominant failure
signature in that run: the same
`java.lang.NullPointerException: Cannot invoke "String.getBytes(java.nio.charset.Charset)"
because "statementName" is null` at `QueryExecutorImpl.sendCloseStatement`,
across a huge, otherwise-unrelated cross-section of the suite (batch
operations, composite IDs, collection mapping, and more) — anywhere pgjdbc
re-prepares a reused `PreparedStatement`.

## Symptom

```
java.lang.NullPointerException: Cannot invoke "String.getBytes(java.nio.charset.Charset)" because "statementName" is null
	at org.postgresql.core.v3.QueryExecutorImpl.sendCloseStatement(QueryExecutorImpl.java:2041)
	at org.postgresql.core.v3.QueryExecutorImpl.processDeadParsedQueries(QueryExecutorImpl.java:2280)
	at org.postgresql.core.v3.QueryExecutorImpl.sendQueryPreamble(QueryExecutorImpl.java:698)
	at org.postgresql.core.v3.QueryExecutorImpl.execute(QueryExecutorImpl.java:649)
	at org.postgresql.jdbc.PgStatement.internalExecuteBatch(PgStatement.java:900)
	...
```

`QueryExecutorImpl.processDeadParsedQueries()` polls a `ReferenceQueue`,
looks the dequeued `PhantomReference` up in a
`HashMap<PhantomReference<SimpleQuery>, String> parsedQueryMap` to recover
the server-side prepared-statement name, and closes that statement.
`Nullness.castNonNull` immediately after the `HashMap.remove` call is a pure
compile-time type assertion (confirmed via `javap`), not a runtime null
check, so a `null` map lookup silently flows into `sendCloseStatement` and
NPEs two frames later on `statementName.getBytes(...)`.

## Root cause — two independent bugs in the same code paths

Reading the real pgjdbc 42.7.11 bytecode (`javap -p -c` on
`QueryExecutorImpl`/`SimpleQuery`) showed the actual mechanism is
`Reference.enqueue()` called **explicitly by application code while the
referent is still reachable** — not the GC discovering a dead referent:

```java
// SimpleQuery.setCleanupRef / unprepare()
PhantomReference<SimpleQuery> old = this.cleanupRef;
if (old != null) {
    old.clear();
    old.enqueue();          // <-- explicit, immediate enqueue
}
this.cleanupRef = newRef;   // a NEW PhantomReference wraps the SAME, still-alive SimpleQuery
```

A long-lived, reused `PreparedStatement` gets re-prepared many times over
its life (`sendParse` calls `query.unprepare()` before re-registering), so
the *same* `SimpleQuery` referent ends up wrapped by many successive
`PhantomReference` instances, each explicitly retired via `clear()` +
`enqueue()` the moment it's superseded — while `query` itself stays alive
through the next generation.

A minimal Java-only repro reproducing this exact shape,
`old.clear(); old.enqueue();` on a `PhantomReference` whose referent is
still reachable, immediately followed by `queue.poll()` +
`map.remove(polled)`, found **two** distinct CratonVM defects:

### Bug 1 — `ReferenceQueue.poll()` doesn't recognize the JDK's self-loop "empty queue" sentinel, so it redelivers the same `Reference` twice

The real JDK's `ReferenceQueue.enqueue0` bytecode uses a **self-referential**
`next == r` sentinel — not `null` — to mark "`r` was the only/last element
enqueued into an otherwise-empty queue":

```java
r.next = (head == null) ? r : head;
head = r;
```

`java.lang.ref.PhantomReference`/`Reference` and `ReferenceQueue.<init>`
have native overrides in CratonVM, but `Reference.enqueue()`'s real-JDK-layout
path (`native_ref_enqueue` in `native-builtins/src/reference.rs`) correctly
delegates the actual linkage to the **real** JDK bytecode
(`invoke_special("java/lang/ref/ReferenceQueue", "enqueue", ...)`) — so a
`Reference` enqueued while the queue was empty legitimately arrives with
`next == ref_obj` (itself). `ReferenceQueue.poll()`, however, is a **native
override** (`native_rq_poll`) that unconditionally republished `next` as the
new head. For the self-loop case that means: after popping `ref_obj`, `head`
is set right back to `ref_obj` — the object polled off the queue moments ago.
The *next* `poll()` call finds "an entry" again and delivers the same,
already-fully-processed `Reference` a second time.

Confirmed with a standalone repro
(`old.clear(); old.enqueue(); queue.poll(); queue.poll();`): CratonVM's
second `poll()` returned the identical `ObjectRef`/identity-hash as the
first, whereas real HotSpot correctly returns `null` after one delivery.

### Bug 2 — the GC's own reference registry never learns about an explicit `Reference.enqueue()`, so it can independently redeliver the same Reference a THIRD time

Every `PhantomReference` — including one about to be explicitly retired —
is also registered with the GC's `ReferenceProcessor` at construction
(`discover_reference`), which tracks its own `enqueued`/`cleared` flags
completely independently of the Java-level linked list.
`native_ref_enqueue` never told the processor that the application had
already enqueued and (via the app's own bookkeeping) fully drained the
reference. So the stale registry entry — `enqueued: false` — sat there,
still pointing at the shared, still-alive `SimpleQuery` referent, until
`remove_collected` eventually pruned it once the `PhantomReference` object
itself died. If the shared referent (the reused `SimpleQuery`) died first —
entirely realistic for a repeatedly-reprepared statement whose statement
object finally goes out of scope — `ReferenceProcessor::process_phantom_refs`
would rediscover *every* still-registered, not-yet-pruned entry pointing at
that referent (one per historical re-prepare generation) and enqueue all of
them, including the ones the application had already retired, removed from
`parsedQueryMap`, and forgotten about.

Both bugs produce the identical observable symptom — `queue.poll()`
legitimately returns non-null, but `parsedQueryMap.remove(ref)` returns
`null` because the map entry for that exact `Reference` was already
consumed by an earlier, correct delivery.

## Fix

**Bug 1** — `native-builtins/src/reference.rs`, `native_rq_poll`: after
reading `ref_obj`'s `next` field, normalize the self-loop sentinel to "queue
now empty", matching real JDK `poll0`:

```rust
let next = ctx.get_field(ref_obj, next_slot);
let next = match next {
    Value::Object(Some(n)) if n == ref_obj => Value::Object(None),
    other => other,
};
ctx.set_field(this, RQ_FIELD_HEAD, next);
```

**Bug 2** — a new `ReferenceProcessor::mark_manually_enqueued` in
`gc/src/reference.rs` retires the registry entry (sets `cleared = true`,
`enqueued = true`, without touching `pending_queues` — the application
already performed the real enqueue) for a `Reference` whose `reference_obj`
address matches. Wired through a new `NativeContext` method
(`mark_reference_manually_enqueued`, default no-op, implemented in
`vm/src/vm/vm_exec.rs`) and called from `native_ref_enqueue` in
`native-builtins/src/reference.rs` immediately after a successful explicit
enqueue:

```rust
if matches!(&result, Ok(Some(Value::Int(1)))) {
    let this = ctx.read_native_pin(this_pin, this);
    ctx.mark_reference_manually_enqueued(this);
}
```

Both fixes are independently necessary: Bug 1 alone would still leave a
redundant registry entry to be rediscovered later; Bug 2 alone would still
double-deliver on the very first explicit enqueue via the self-loop.

## Verification

Standalone repros (`scratchpad/PhantomGhostProbe.java`,
`PhantomGhostProbeSmall.java` — 200 long-lived referents each re-prepared 30
times, mirroring `SimpleQuery.setCleanupRef`/`unprepare()` exactly, with an
immediate `queue.poll()`+`map.remove()` drain after each re-prepare and a
final forced-GC drain):

- Pre-fix: `SUMMARY: numStmts=200 reprepareCount=30 found=5999 mismatched=5800 map.remaining=1 STATUS=FAIL`
- Post-fix: `SUMMARY: numStmts=200 reprepareCount=30 found=5999 mismatched=0 map.remaining=1 STATUS=PASS`
- Real HotSpot on the identical repro: `found=6000 mismatched=0 STATUS=PASS` (baseline, always passed)

Original failing class, against a scratch Postgres database
(`hibernate_orm_test_agent_scratch`, isolated from the concurrently-running
full-suite shards):

- Pre-fix: `@@RESULT org.hibernate.orm.test.action.queue.BatchSizeExceedingTest found=9 started=9 ok=7 failed=2` — both failures the exact `statementName is null` NPE.
- Post-fix: `@@RESULT org.hibernate.orm.test.action.queue.BatchSizeExceedingTest found=9 started=9 ok=9 failed=0 aborted=0 skipped=0` — `@@BATCHEND failed_classes=0`.

Regression (`--target-dir target-agentfix`, built binary kept separate from
the full-suite run's canonical `target/release/cratonvm.exe` path so as not
to disturb the concurrently-running suite):

- `cargo test --release -p cratonvm-gc --lib`: **972 passed, 0 failed**.
- `cargo test --release -p cratonvm-vm --lib -- reference hash`: **26 passed, 0 failed** (0 tests match a `phantom`-named filter in this crate).
- `cargo test --release -p cratonvm-native-builtins --lib -- reference`: **20 passed, 0 failed**.

## Related

Root-caused by reading the real pgjdbc 42.7.11 bytecode directly
(`javap -p -c` on `QueryExecutorImpl`, `SimpleQuery`, and the JDK 25
`java.lang.ref.ReferenceQueue`/`Reference` classes) rather than guessing from
the stack trace — the NPE site itself (`sendCloseStatement`) is two frames
downstream of the actual defect, and a same-shape single-`PhantomReference`
repro (referent dies once, is discovered by the GC, enqueued once) does
**not** reproduce either bug; the repro needs the "retire while still alive,
replace with a new wrapper" pattern real pgjdbc uses for a reused
`PreparedStatement`.
