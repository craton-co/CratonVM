# `TestUpgrade` regressed: `Throwable.initCause` cascade — `IllegalStateException: Can't overwrite cause with a null`

## Status
**OPEN, confirmed CratonVM-specific (differential-verified against real HotSpot)** —
found 2026-08-14 on a serial (single-shard, `--shard 1/1`) full 218-class suite
run on the second Azure host (`azureuser@20.80.105.49`, worktree
`/data/cvm-h2serial-20260813`, branch `test/h2-full-serial-20260813` off
`origin/dev` @ `464280e14`).

This is a **regression of `org.h2.test.unit.TestUpgrade`** — the exact class
the entire 12-pass investigation in the (now-closed)
[`bug-h2-suite-residual-fail-triage-FIXED.md`](../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-suite-residual-fail-triage-FIXED.md)
root-caused and fixed. That fix (a `try_stackless_invoke` dispatch bug) is
**not** what's broken here — this is a different, later-introduced defect that
happens to land on the same class.

## The failure
```
Exception in thread "main" java/lang/IllegalStateException: Can't overwrite cause with java.lang.IllegalStateException: Can't overwrite cause with java.lang.IllegalStateException: Can't overwrite cause with java.lang.IllegalStateException: Can't overwrite cause with java.lang.IllegalStateException: Can't overwrite cause with a null
	at org/h2/test/unit/TestUpgrade.main(TestUpgrade.java:35)
	at org/h2/test/TestBase.testFromMain(TestBase.java:479)
	at org/h2/test/unit/TestUpgrade.test(TestUpgrade.java:41)
	at org/h2/test/unit/TestUpgrade.testUpgrade(TestUpgrade.java:59)
	at org/h2/Driver.connect(Driver.java:60)
	at org/h2/message/Message.convert(Message.java:283)
	at org/h2/message/Message.getSQLException(Message.java:106)
	at org/h2/jdbc/JdbcSQLException.<init>(JdbcSQLException.java:53)
Caused by: org/h2/jdbc/JdbcSQLException: General error: java.lang.IllegalStateException: Can't overwrite cause with ... (4 more nested levels, same message, each wrapping the next)
```

Five levels of the identical `IllegalStateException` message, each nested as
the `cause` of the next `JdbcSQLException`/`IllegalStateException` pair,
bottoming out in `Database.openDatabase` -> `Engine.openSession` ->
`SessionFactoryEmbedded.createSession` -> `SessionRemote.createSession` ->
`JdbcConnection.<init>` -> `Driver.connect`. Reproduces **deterministically**
— reran standalone twice, identical failure both times, ~0.1s in (fast, not
a timeout).

**Differential check against real HotSpot JDK 25** (same `run-h2-suite.sh
hotspot` harness, same classpath, same H2 checkout):
```
[hotspot] PASS          5.8s org.h2.test.unit.TestUpgrade
```
Clean pass. This confirms the failure is CratonVM-specific, not an
H2-vs-JDK25 compatibility issue.

## Likely trigger
`native_throwable_init_cause` (`native-builtins/src/lang_misc.rs`) was
changed in `180ceeb8e` ("fix(throwable/vm): the Throwable state machine and
HotSpot's cast/store wording", 2026-08-11) from an unconditional setter to a
proper state machine that throws `IllegalStateException` when `cause` is
already set — matching HotSpot's documented `Throwable.initCause` contract.
Before that commit, `initCause` was silently a no-op setter (per that
function's own doc comment: "a differential probe measured
`Throwable.initCauseAfterCtorThrows`... as `no-throw` where HotSpot
raises").

That refusal is correct per spec and confirmed to match HotSpot in isolation
(same doc comment: "CratonVM's `cause` field already tracks HotSpot's
exactly"). What this class's failure shows is that **something in
CratonVM's execution of H2's exception-wrapping path
(`Message.convert`/`getSQLException`, the `JdbcSQLException` constructor
chain) calls `initCause()` an extra time (or in a different order) than the
same bytecode does under real HotSpot** — so the newly-strict refusal now
fires on a call that HotSpot's execution of the identical Java source never
makes. The previous unconditional-setter behavior was silently masking this
extra/duplicate call; enforcing the real contract surfaced it as a hard
failure instead.

This is the same shape of finding as this project's own `docs-known-issues-
convention` caution: honoring a previously-loose check reveals a **different**
latent defect rather than being itself the defect (see also `a-never-
honoured-flag-hides-3-landmines`-class findings elsewhere in this repo's
history). The fix commit itself is not wrong; something upstream of it is
calling `initCause` more often than real bytecode execution would.

## What's not yet known
* Which of the 5 nested wrap points is the FIRST spurious `initCause` call —
  i.e. where CratonVM's call count diverges from HotSpot's. The 5-deep
  nesting suggests each JDBC-layer catch-and-rewrap step
  (`SessionFactoryEmbedded` -> `SessionRemote` -> `JdbcConnection` ->
  `Message.convert`/`getSQLException` -> `Driver.connect`) independently
  triggers one more `initCause`, but that's inference from the stack shape,
  not confirmed against a per-call trace.
* Whether this affects only `TestUpgrade`'s specific old-version-database-open
  path, or any H2 code path that catches-and-rewraps an exception through
  multiple JDBC layers (which would make this a broader-reaching defect than
  just this one class).
* Whether the divergence is in interpreter dispatch (an extra invocation of
  the constructor chain) or in a native (something CratonVM-side calling
  `initCause` directly that HotSpot's real bytecode does not).

## Next steps
* Add a debug trace to `native_throwable_init_cause` (temporary,
  env-gated) that dumps the caller's frame chain on every invocation for a
  `TestUpgrade`-only run, and diff the resulting call count/order against
  what the JDK source of `Message.convert`/`JdbcSQLException`'s constructors
  implies should happen exactly once per real exception wrap.
* Check whether `Message.convert`'s own retry logic (`Driver.connect` is
  itself in the trace twice, at both `Message.java:283` entries) is being
  re-entered under CratonVM in a way it should not be — e.g. a caught
  exception being re-thrown and re-caught by an outer handler that HotSpot's
  bytecode does not reach.
* Re-run this class (and TestUpgrade specifically) after any fix using the
  same `hotspot` vs `--nojit` A/B this doc used to confirm the fix closes the
  gap without reopening the original `initCause` differential probe's find.

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home <jdk25-home> --nojit --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.unit.TestUpgrade
```
Fails in well under 1 second, deterministically.

## Related
* [`bug-h2-suite-residual-fail-triage-FIXED.md`](../../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-suite-residual-fail-triage-FIXED.md)
  — the original 12-pass `TestUpgrade` investigation. That fix is unrelated
  and still correct; this is a new, later-introduced defect on the same
  class.
* `native-builtins/src/lang_misc.rs`'s `native_throwable_init_cause` — the
  state-machine implementation whose newly-correct strictness surfaced this.
