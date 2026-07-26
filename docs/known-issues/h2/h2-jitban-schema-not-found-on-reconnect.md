# H2 JIT-ban lift (`org/h2/`, HIB-LONGTAIL.1) exposes systemic "Schema  not found" corruption on DB reopen/meta-record replay — ban must stay

**Status:** OPEN, unfixed. Confirmed via a 218-class full-suite differential
run, not a single-test artifact. Not yet root-caused to an exact JIT codegen
site.

## Context

Part of the 2026-07-25/26 "jit-ban-sweep" effort re-testing
`vm/src/jit/skip_list.rs`'s app-specific bans now that the general
callee-saved-GPR-clobber fix (2026-07-04) and other JIT rework have landed.
`HIB-LONGTAIL.1`'s blanket ban on `org/h2/` + `org/antlr/v4/runtime/`
(skip_list.rs ~L871) is framed purely as a **performance** safety net in its
own comment ("turned ordinary 9-second HotSpot tests into multi-minute
CratonVM runs"). Testing it by lifting via
`CRATONVM_JIT_ALLOW_PACKAGES=org/h2/,org/antlr/v4/runtime/` and running the
full 218-class H2 suite (`apps/h2database-suite-runner/run-h2-suite.sh`,
4-way sharded, real JDK, `--Xmx 1g`) shows it is **also hiding a genuine
correctness bug**, not just a throughput one.

## Result (partial run — 99/218 classes completed before this session's
background job was interrupted; still a large, representative sample)

```
PASS=53  HANG=6  FAIL=40
```

Compare to the existing ban-in-place baseline (full 218 classes,
`apps/h2database-suite-runner/RESULTS-20260724.md`):
```
PASS 143 (65.6%)  HANG 56 (25.7%)  FAIL 19 (8.7%)
```

The HANG rate dropping (6/99 ≈ 6% vs. baseline's 25.7%) is consistent with
the ban's own stated perf rationale — lifting it does seem to fix some
hangs. **But FAIL jumps from 8.7% to ~40%**, and it is not diffuse noise:

```
16 / 40 FAILs (40%) are the exact same signature:
  org.h2.jdbc.JdbcSQLSyntaxErrorException: Schema  not found
```
(note: "Schema" followed by two spaces then "not found" — the schema *name*
being looked up is blank/empty, not merely absent from the catalog).

## Signature

Hits across completely unrelated test areas — `TestLob`, `TestDeadlock`,
`TestFullText`, `TestOptimizations`, `TestTransaction`, `TestViewAlterTable`,
`TestPreparedStatement`, `TestRunscript`, `TestAnalyzeTableTx`,
`TestCompatibilityOracle`, `TestSequence`, `TestPowerOff`, `TestScript`,
`TestUpdatableResultSet`, `TestViewDropView`, `TestSynonymForTable` — the
common thread is **any test that reopens/reconnects to a persisted
database**, which triggers metadata replay. Representative trace
(`TestLob`):

```
Exception in thread "main" org/h2/jdbc/JdbcSQLSyntaxErrorException: Schema  not found
	at org/h2/test/db/TestLob.reconnect(TestLob.java:1117)
	at org/h2/test/TestDb.getConnection(TestDb.java:31)
	...
	at org/h2/engine/Database.<init>(Database.java:361)
	at org/h2/engine/Database.executeMeta(Database.java:652)
	at org/h2/engine/Database.executeMeta(Database.java:680)
	at org/h2/engine/MetaRecord.prepareAndExecute(MetaRecord.java:72)
	at org/h2/engine/SessionLocal.prepare(SessionLocal.java:577)
	at org/h2/command/Parser.prepare(Parser.java:455)
	at org/h2/command/Parser.parse(Parser.java:584)
	at org/h2/command/Parser.parsePrepared(Parser.java:648)
	at org/h2/command/Parser.parseCreate(Parser.java:6436)
	at org/h2/command/Parser.parseCreateSequence(Parser.java:6761)
	at org/h2/command/Parser.getSchema(Parser.java:932)
	at org/h2/command/Parser.getSchema(Parser.java:926)
```

Fails while replaying a persisted `CREATE SEQUENCE ...` metadata record
during `Database`'s constructor (opening/reopening an existing database) —
`Parser.getSchema` resolves the (implicit, current) schema by name and gets
an empty string instead of the expected default schema name (`PUBLIC` in
most of these tests). This is consistent with the "allocate-then-putfield"
archetype already catalogued across ~25 other bans in this file (a String
field — most likely `Parser`'s or `SessionLocal`'s current-schema-name slot,
or `Database`'s default-schema reference — not correctly populated by the
time it's read back under JIT), though not yet confirmed to be exactly that
mechanism.

Two other FAIL signatures worth a second look but far less frequent (1-2
occurrences each, may be downstream effects of the same corruption or
independent): `NullPointerException` in `sun.nio.ch.Interruptible.interrupt`
(via MVStore), and an `OutOfMemoryError` in `TestOpenClose`
(`anewarray component 823 length 0`) plus a separate young-gen-exhausted
`FATAL` in `TestLargeBlob` — these two OOM cases could be a real memory
corruption (a corrupted array-length operand from the same class of bug) or
could be incidental to the very heavily loaded host this ran on; not
conclusively tied to the Schema-not-found root cause.

## Disposition

**Do not lift `HIB-LONGTAIL.1` (`org/h2/`)** — despite the ban comment's
purely-perf framing, there is a live correctness bug behind it. The
`org/antlr/v4/runtime/` half of this same ban was not independently
isolated this session (always tested together) — worth a separate pass once
the H2 side is understood, since ANTLR is only used by H2's own parser
generator paths in a few places, not the bulk of the schema/reconnect path
implicated here.

## Reproduction

```bash
# binary and H2 checkout paths as used this session:
#   binary: any cratonvm-cli release build
#   H2_ROOT=/data/data/h2database/h2 (pre-built via `run-h2-suite.sh setup`)
cd apps/h2database-suite-runner
TMPDIR=/data/tmp H2_ROOT=/data/data/h2database/h2 CRATONVM_BIN=<binary> \
  CRATONVM_JIT_ALLOW_PACKAGES='org/h2/,org/antlr/v4/runtime/' \
  ./run-h2-suite.sh run --category all --only 'TestLob|TestSequence|TestPowerOff'
```
(`TMPDIR=/data/tmp` is required on this host — root `/` is out of space and
`run-h2-suite.sh`'s internal `mktemp` silently produces empty results
otherwise; see the `azure-host-disk-full-flapping` memory / this session's
`docs/internal/jit-ban-sweep-20260725.md` for detail.)

Single-class repro is much faster for iterating (`TestSequence` or
`TestPowerOff` reconnect quickly): use `--only 'TestSequence'`.

## Related

- `docs/internal/jit-ban-sweep-20260725.md` — the sweep tracking doc this
  was found under; also documents a second, independently-confirmed still-
  needed ban in the same sweep (`org/jboss/as/`, WildFly boot
  `ModelTypeValidator.validTypes` NPE — see
  `docs/known-issues/wildfly/modeltypevalidator-validtypes-npe.md`).
- `apps/h2database-suite-runner/RESULTS-20260724.md` — the ban-in-place
  baseline this result was compared against.
