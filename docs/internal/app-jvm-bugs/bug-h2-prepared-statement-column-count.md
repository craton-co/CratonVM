# H2 — prepared-statement column count mismatch

## Status
**FIXED** (dev `ec4f328`, 2026-06-05) — was a **downstream symptom of the ArrayDeque.remove desync** (see `bug-h2-mvstore-sys-lock-timeout.md`), not an independent bug: corrupted `MVTable.waitingSessions` → SYS lock timeouts → CREATE/DROP TABLE never committed → stale schema → INSERT "column count does not match". Fixed by registering `ArrayDeque.remove(Object)` / `removeFirstOccurrence` / `removeLastOccurrence`. Verified: **0** `Column count does not match` in `TestScript --nojit` (was 18 occurrences).

## Severity
**HIGH / correctness** — wrong SQL execution semantics in script regression tests.

## App / suite
- **App:** H2 Database (`apps/h2database`)
- **Suite:** `org.h2.test.TestAll` → `org.h2.test.scripts.TestScript`
- **Logs:** `test-infra/suite-results/apps-four-20260604-233229/h2-testall.log` (18 occurrences)

## Symptom

```
org.h2.jdbc.JdbcSQLSyntaxErrorException: Column count does not match
```

Typical stack:

```
JdbcPreparedStatement.execute
  → CommandContainer.update
  → … TestScript …
```

## HotSpot behavior

Not observed in the same bounded HotSpot run (HotSpot exits earlier on missing PostgreSQL driver). Expected behavior on HotSpot for these scripts: statements bind the correct number of columns.

## CratonVM behavior

Repeated `Column count does not match` during `TestScript`. Likely causes downstream `Table "…" not found` errors when DDL/DML leaves the test database in a bad state.

## Root cause (suspected)

Mismatch between **prepared statement metadata** (expected column count) and **bound parameter count** or **INSERT/UPDATE row shape** at execution time. Possibilities:

1. Wrong `ResultSetMetaData` / parameter metadata from JDBC layer
2. Interpreter bug in `PreparedStatement.set*` / `executeUpdate` operand handling
3. SQL parser producing a different column list than HotSpot for the same script

**Suspect areas:** JDBC prepared-statement natives, expression evaluation in DML, array/multi-row insert paths.

## Impact

- Script-based H2 tests fail en masse.
- Secondary DDL failures (missing tables) are artifacts of this bug, not separate root causes.

## Reproduce

Run full `TestAll` (see [bug-h2-securerandom-sha1prng.md](bug-h2-securerandom-sha1prng.md)) and grep:

```bash
grep -i "Column count does not match" h2-testall.log
```

Bisect to a single `TestScript` case once SHA1PRNG is fixed so the suite runs longer.

## What to fix

1. Identify the first failing script statement (test name + SQL snippet from H2 test output).
2. Compare prepared-statement column count vs bind count under CratonVM vs HotSpot (logging or micro-probe).
3. Fix JDBC/SQL layer so column arity matches HotSpot.
4. Re-run `TestScript` subset, then full `TestAll`.

## Related

- [bug-h2-mvstore-sys-lock-timeout.md](bug-h2-mvstore-sys-lock-timeout.md)
- `apps/h2database/CRATONVM_BUGS.md` Bug 4 (cascade DDL — downstream only)
