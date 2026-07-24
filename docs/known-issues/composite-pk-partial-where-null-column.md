# Composite primary key + partial-key `WHERE` returns non-key column values as raw `null` (NOT `ValueNull.INSTANCE`)

## Status
**OPEN, discovered 2026-07-24, not root-caused.** General, H2-internals-independent
CratonVM correctness bug — not specific to any particular application suite.
Discovered as a side effect of the H2 `TestUpgrade` Parser-loader-collapse fix
session (see `docs/known-issues/h2-suite-bugs/bug-h2-suite-residual-fail-triage.md`'s
ninth-pass section) while investigating a deeper `TestUpgrade` LOB-migration
residual, and very likely the same root cause as a separately-observed dev
regression in `org.h2.test.db.TestLinkedTable` / `org.h2.test.jdbc.TestPreparedStatement`
(both also show a raw `null` reaching a `Value` receiver in a JDBC result row —
see `h2-nullvalue-in-row-dev-regression-20260724` in this repo's memory system).

## Minimal repro (H2, but the mechanism is almost certainly general — MVStore's
## own pure-Java B-tree/cursor code, not any H2-specific native override)

```java
Class.forName("org.h2.Driver");
try (Connection conn = DriverManager.getConnection("jdbc:h2:/tmp/db", "sa", "")) {
    Statement stat = conn.createStatement();
    stat.execute("CREATE TABLE T(ID INT, PART INT, NAME VARCHAR, PRIMARY KEY(ID, PART))");
    stat.execute("INSERT INTO T VALUES(0, 0, 'hello')");
    try (ResultSet rs = stat.executeQuery("SELECT NAME FROM T WHERE ID=0")) {
        while (rs.next()) {
            System.out.println("NAME=" + rs.getString(1));  // NPEs on CratonVM
        }
    }
}
```

**On real HotSpot JDK25**: prints `NAME=hello`, exits cleanly.

**On CratonVM (`--nojit`, ruling out JIT compilation as a cause)**: throws
```
NullPointerException: Cannot invoke "org.h2.value.Value.getString()" because "..." is null
	at org/h2/jdbc/JdbcResultSet.getString(JdbcResultSet.java:283)
```

The table's own PRIMARY KEY (`ID`, `PART`) columns come back **correct** when
read directly (verified: `rs.getInt(1)`/`rs.getInt(2)` on a `SELECT ID, PART,
... FROM T WHERE ID=0` both print the right values, `0` and `0`) — only
`NAME`, the first NON-key column, comes back as a raw Java `null` in the
underlying `Value[]` row array (`JdbcResultSet.getInternal`'s
`list[columnIndex - 1]` — confirmed via direct inspection, not inferred from
the NPE alone) rather than the `ValueNull.INSTANCE` sentinel H2 always uses
for actual SQL NULL, or the real stored value.

## What's confirmed so far (2026-07-24 investigation)

- **The `WHERE` clause is required.** `SELECT NAME FROM T` (no `WHERE`) and
  `SELECT NAME FROM T ORDER BY PART` (no `WHERE`, forces a scan+sort) both
  work correctly. `SELECT NAME FROM T WHERE ID=0` (no `ORDER BY`) alone is
  sufficient to reproduce — `ORDER BY` is NOT required.
- **A composite (multi-column) primary key is required**, with the `WHERE`
  clause matching only a PREFIX of it (`WHERE ID=0`, not `WHERE ID=0 AND
  PART=0`) — i.e. a genuine index RANGE scan (all rows whose key starts with
  `ID=0`), not a single-row exact-key point lookup. A single-column
  `PRIMARY KEY(ID)` with `WHERE ID=0` (an exact point lookup) works fine —
  tested directly, does NOT reproduce.
- **Not JIT-related**: reproduces identically under `--nojit`.
- **Not table-name-specific, not H2-app-specific**: reproduces with a plain
  user table (`T`), not just H2's own internal `SYSTEM_LOB_STREAM` staging
  table (where this was first observed, inside `Upgrade.upgrade()`'s
  RUNSCRIPT-based data migration — see the H2 doc referenced above).
- **Not size-dependent**: reproduces with a 1-character `VARCHAR` value and
  with multi-KB `BINARY` values equally.
- **No native-builtins override is involved** — grepped `apps_h2.rs` and the
  rest of `native-builtins/` for any registration touching
  `org/h2/mvstore/*`/`org/h2/index/*`/cursor-related classes: zero hits. This
  strongly suggests the bug is in the INTERPRETER's execution of H2's own
  pure-Java MVStore B-tree/cursor code (`MVMap`/`IndexCursor`/
  `MVSecondaryIndex` family, or wherever CratonVM implements the equivalent
  range-scan machinery), not an app-specific native shim — i.e. a real,
  general interpreter/collections correctness bug, not narrowly an "H2
  support" gap.

## Not yet done / next steps for whoever picks this up

- Root cause not identified. The next step is almost certainly to trace
  H2's own `MVSecondaryIndex`/`IndexCursor`/`Cursor` classes' row-fetch path
  for a partial-composite-key range scan specifically (as opposed to a
  full scan or an exact-key point lookup, both confirmed clean) — likely by
  instrumenting wherever CratonVM's interpreter or MVStore-backing storage
  reconstructs a row's non-key `Value[]` payload during an index-range
  cursor `next()`/`get()` call, gated behind a new env-var-controlled trace
  tag (following this repo's established `CRATONVM_DBG_*` convention).
- Given the very plausible connection to the separately-flagged dev
  regression (`TestLinkedTable`'s `GROUP BY`, `TestPreparedStatement`'s
  subquery — see `h2-nullvalue-in-row-dev-regression-20260724` in memory,
  spawned task `task_2168068f`), whoever picks up either should check
  whether fixing this closes both, rather than investigating twice.
- Given the general nature of this bug (composite PK + prefix WHERE is an
  extremely common query shape), it's worth a quick spot-check across other
  suites (Spring/Hibernate JPA composite `@IdClass`/`@EmbeddedId` entities,
  WildFly datasources, etc.) for silently-passing-until-now false negatives
  once root-caused — this may explain otherwise-unexplained null-related
  failures elsewhere in this codebase's suite runs.

## Repro files

Not committed (throwaway probes) — recreate from the minimal repro above, or
see `/tmp/lobrepro/CompositeKeyLookup.java` on the Azure host (session-local,
not preserved across host reboots) for the exact working version used during
this investigation.
