# `TestTempTables` hits `CloneNotSupportedException` via a nonsensical `java.lang.Thread.clone` stack frame

## Status
**OPEN** — flagged via an anomalous stack trace; exact mechanism not
root-caused. Found while re-running the H2 suite's HANG classes with a
longer (1500s) per-class timeout.

## Severity
**MEDIUM** — `testLotsOfTables` is a scale test (creates many temp tables);
this fails partway through, and the stack trace itself is evidence of a
correctness bug in exception/stack-trace construction, dispatch, or both.

## Affected test class
`org.h2.test.db.TestTempTables` (`testLotsOfTables`)

## Symptom
```
org.h2.jdbc.JdbcSQLNonTransientException: General error: "java.lang.CloneNotSupportedException"
Caused by: java/lang/CloneNotSupportedException
	at org/h2/test/db/TestTempTables.main(TestTempTables.java:31)
	at org/h2/test/TestBase.testFromMain(TestBase.java:479)
	at org/h2/test/db/TestTempTables.test(TestTempTables.java:50)
	at org/h2/test/db/TestTempTables.testLotsOfTables(TestTempTables.java:307)
	at org/h2/jdbc/JdbcStatement.executeUpdate(JdbcStatement.java:147)
	at org/h2/jdbc/JdbcStatement.executeUpdateInternal(JdbcStatement.java:196)
	at org/h2/command/Command.executeUpdate(Command.java:251)
	at org/h2/command/Command.executeUpdate(Command.java:307)
	at org/h2/command/CommandContainer.update(CommandContainer.java:139)
	at org/h2/command/ddl/CreateTable.update(CreateTable.java:112)
	at org/h2/schema/Schema.createTable(Schema.java:797)
	at org/h2/mvstore/db/Store.createTable(Store.java:223)
	at org/h2/mvstore/db/MVTable.<init>(MVTable.java:158)
	at org/h2/mvstore/db/MVPrimaryIndex.<init>(MVPrimaryIndex.java:53)
	at org/h2/mvstore/db/MVTable.getTransactionBegin(MVTable.java:649)
	at org/h2/mvstore/tx/TransactionStore.begin(TransactionStore.java:460)
	at org/h2/mvstore/tx/TransactionStore.begin(TransactionStore.java:473)
	at org/h2/mvstore/tx/TransactionStore.registerTransaction(TransactionStore.java:499)
	at org/h2/mvstore/tx/VersionedBitSet.<init>(VersionedBitSet.java:25)
	at org/h2/mvstore/tx/BitSetHelper.flip(BitSetHelper.java:34)
	at java/util/Arrays.copyOf(Arrays.java:3617)
	at java/lang/Thread.clone(Thread.java:1037)
```

## Why this trace is suspicious
The bottom (innermost) two frames don't form a plausible real call chain:
`java.util.Arrays.copyOf` does not call `java.lang.Thread.clone`, and
`java.lang.Thread` neither implements `Cloneable` nor declares its own
`clone()` override (it would inherit `Object.clone()` if called at all,
which real code essentially never does). `BitSetHelper.flip` calling into
`VersionedBitSet`/`BitSet`'s **real** `clone()` implementation would
plausibly call `Arrays.copyOf` (that's exactly how real `BitSet.clone()`
copies its internal `long[] words` array) — but the frame immediately
below that copy, which should be `BitSet.clone()` itself (or
`Object.clone()`, if the shallow-copy step is what actually throws), is
instead attributed to an unrelated class (`Thread`) that has no business
appearing here at all.

This has the same *shape* as several already-understood bug families in
this codebase (a caller frame's declaring class being misattributed —
matching this doc's own twelfth-pass `TestUpgrade` fix earlier in this
session, and the already-fixed "array `clone()` dispatches through the
component class instead of `Object`" bug documented directly in
`vm/src/runtime/interpreter/invoke.rs`'s own `try_stackless_invoke` — see
its `T15` comment on array-typed receivers). Neither of those exact fixes
applies here (the receiver is a real `BitSet` subclass, not an array), but
the pattern — a native/dispatch mechanism attributing a clone-related
frame to the wrong class — is a plausible starting hypothesis, not a
confirmed one.

## Not yet done
- Did not add targeted tracing to confirm whether `VersionedBitSet`/`BitSet`
  is genuinely dispatched to `Object.clone()`'s native (`native_object_clone`
  in `native-builtins/src/lib.rs`) with the wrong receiver, or whether the
  `Thread.clone` frame is purely a stack-trace-construction artifact (the
  real fault happens elsewhere and this frame is fabricated/stale when the
  exception's `StackTraceElement[]` gets built) — these would need
  different fixes.
- Did not check whether `CloneNotSupportedException` is thrown by real
  `BitSet.clone()` bytecode itself (which it shouldn't be — `BitSet`
  implements `Cloneable`) or by a native shim.
- A separate `CloneNotSupportedException` was also observed this session in
  `org.h2.test.synth.TestMultiThreaded` (a randomized/non-deterministic
  synth test whose exact failure varies run to run — a *different* run of
  the same class instead hit an unrelated `NullPointerException:
  ConditionAndOr.right`), so that occurrence is **not** confirmed to share
  this root cause and its stack wasn't captured in equivalent detail —
  noted as a possible second data point, not proof of a pattern.

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 --nojit \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.db.TestTempTables
```
Reproduced once this session (~170s); not yet confirmed deterministic
across repeated runs.
