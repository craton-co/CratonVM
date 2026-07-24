# Composite primary key + partial-key `WHERE` returns non-key column values as raw `null` (NOT `ValueNull.INSTANCE`) — FIXED

## Status
**FIXED, 2026-07-24, commit `a88d06a5a` on `dev`.** General,
H2-internals-independent CratonVM correctness bug — not specific to any
particular application suite. Discovered as a side effect of the H2
`TestUpgrade` Parser-loader-collapse fix session (see
`../../../known-issues/h2/bug-h2-suite-residual-fail-triage.md`'s
ninth-pass section) while investigating a deeper `TestUpgrade`
LOB-migration residual, and confirmed to be the same root cause as a
separately-observed dev regression in `org.h2.test.db.TestLinkedTable` /
`org.h2.test.jdbc.TestPreparedStatement` (both now pass).

**Known trade-off, NOT resolved**: fixing this reopened `TestUpgrade`'s
SQL-parsing NPE (previously fixed by a different, unrelated session — see
the main triage doc's ninth-pass section) through an interaction that
significant investigation this session did not root-cause. See the triage
doc's tenth-pass section for the full account and next-step guidance. The
composite-PK fix itself was shipped anyway since it corrects a severe,
general data-correctness bug (silently wrong query results for common
"filter by a prefix of a composite key" queries) affecting far more than
just `TestUpgrade`.

## Minimal repro (H2, but the mechanism is general — a native fast-path
## override for `ExpressionColumn.getValue`, not H2-specific bytecode)

```java
Class.forName("org.h2.Driver");
try (Connection conn = DriverManager.getConnection("jdbc:h2:/tmp/db", "sa", "")) {
    Statement stat = conn.createStatement();
    stat.execute("CREATE TABLE T(ID INT, PART INT, NAME VARCHAR, PRIMARY KEY(ID, PART))");
    stat.execute("INSERT INTO T VALUES(0, 0, 'hello')");
    try (ResultSet rs = stat.executeQuery("SELECT NAME FROM T WHERE ID=0")) {
        while (rs.next()) {
            System.out.println("NAME=" + rs.getString(1));  // NPEs before the fix
        }
    }
}
```

**On real HotSpot JDK25**: prints `NAME=hello`, exits cleanly (always did).
**On CratonVM before the fix** (`--nojit`, ruling out JIT): threw
`NullPointerException: Cannot invoke "org.h2.value.Value.getString()"
because "..." is null` at `JdbcResultSet.getString`. **After the fix**:
prints `NAME=hello` correctly, matching HotSpot.

## Root cause

`native-builtins/src/apps_h2.rs`'s fast-path native override for
`ExpressionColumn.getValue` (registered to avoid reinterpreting
`TableFilter.getValue(Column)`'s bytecode for every nested-range
candidate) read `TableFilter.currentSearchRow` directly and returned
whatever `Row.getValue(columnId)` gave back, with no null check.
`currentSearchRow` is set unconditionally on every `TableFilter.next()`
(`cursor.getSearchRow()`): for a full-table-scan cursor this already IS
the full row (so the fast path was safe and correct there), but for a
cursor driven by a **secondary index over only a PREFIX of a composite
key** (an index range scan, not an exact point lookup — confirmed via a
single-column-PK exact-match test, which does NOT reproduce), it is the
index's own lightweight key row, containing only the INDEXED columns. A
column outside the index legitimately isn't present in that row, and the
real `TableFilter.getValue(Column)` bytecode handles exactly this case
with a lazy fetch:
```java
if (current == null) {
    Value v = currentSearchRow.getValue(columnId);
    if (v != null) return v;
    ...
    current = cursor.get();   // fetch the FULL row, only when needed
    ...
}
return current.getValue(columnId);
```
The native fast path skipped this entire lazy-fetch-on-miss branch,
silently returning the partial row's raw `null` for a non-indexed column
instead.

## Fix

`h2_expression_column_get_value` now only takes the `currentSearchRow`
fast path when the read actually comes back non-null; a null result falls
through to the real (slower, but correct) virtual dispatch
(`resolver.getValue(column)`), which also has the side effect of caching
the fetched full row into `current` for later columns read from the same
row. The extra re-entrant probe call required pinning `resolver`/`column`
across it (both read again afterward) — GC-safety same pattern as every
other re-entrant call in this file; an initial version without the
pinning was tested and behaved identically for the reported bug (pinning
alone did not explain or fix the `TestUpgrade` interaction below).

## Verification

Minimal repro above: fixed. `org.h2.test.db.TestLinkedTable`,
`org.h2.test.jdbc.TestPreparedStatement`: pass. Regression spot-check
(`TestAlter`, `TestShell`, `TestResultSet`, `TestUpdatableResultSet`): all
clean. `org.h2.test.db.TestView`'s pre-existing, unrelated
`testInnerSelectWithRownum` failure (`Expected: 2 actual: 1`) confirmed
present identically on pristine pre-fix `dev` too — not caused by this
fix.
