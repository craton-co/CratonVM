# A foreign key between uncomparable ARRAY types is accepted — the referential existing-data check does not raise

## Status
**FIXED 2026-08-17 — superseded by**
`fixed-suite-bugs/h2-suite-bugs/bug-h2-testscript-fk-array-comparability-skipped-by-rowcount-shortcut-FIXED-20260817.md`.

The root cause was **not** in H2's Java at all: CratonVM natively overrides
`ConstraintReferential.checkExistingData` (`native-builtins/src/apps_h2.rs`), and
that native returns early when the referencing table has no rows — skipping the
`prepare` that H2 relies on for the column-type check. All three "start here"
hypotheses at the end of this record are therefore **wrong**, and wrong in a way
worth keeping: they all assume the Java body executes, which it does not.

The elimination below is kept per this project's convention of preserving
investigation history rather than deleting a superseded hypothesis. It is
accurate as far as it goes — every component really does behave identically on
both VMs — it just never questioned whether the method under investigation was
the code being run.

## The failure

```
ERROR: org/h2/test/scripts/ddl/alterTableAdd.sql
line: 166
exp: > exception TYPES_ARE_NOT_COMPARABLE_2
got: > ok
```

The statement declares a foreign key from an `INTEGER ARRAY` column onto a
`TIME ARRAY` column. H2 is supposed to refuse it. CratonVM creates it.

## Minimal repro

Three statements, no earlier DDL needed — the divergence does not depend on the
rest of the script:

```sql
CREATE TABLE A(A TIMESTAMP PRIMARY KEY, B INT ARRAY UNIQUE, C TIME ARRAY UNIQUE);
CREATE TABLE B(A TIMESTAMP WITH TIME ZONE, B DATE, C INT ARRAY, D TIME ARRAY, E TIME WITH TIME ZONE ARRAY);
ALTER TABLE B ADD FOREIGN KEY(C) REFERENCES A(C);
```

```
HotSpot    Values of types "INTEGER ARRAY" and "TIME ARRAY" are not comparable [90110-249]
CratonVM   accepted; INFORMATION_SCHEMA then shows
             CONSTRAINT_42 col=C posInUnique=1  ->  unique CONSTRAINT_41_0
```

The constraint CratonVM creates points at the *correct* referenced unique
constraint (`CONSTRAINT_41_0` is the `UNIQUE` on `A(C)`, the `TIME ARRAY` one),
so this is not a case of resolving `REFERENCES A(C)` to the wrong column.

## Where the check lives, and what still works

On HotSpot the exception comes from here — note that it is *not* the FK's own
type check:

```
org.h2.value.TypeInfo.checkComparable(TypeInfo.java:766)
org.h2.expression.condition.Comparison.optimize(Comparison.java:168)
   ... Select.prepareExpressions <- Query.prepare <- PredicateWithSubquery.optimize
       <- ExistsPredicate.optimize <- ConditionNot.optimize ...
org.h2.constraint.ConstraintReferential.checkExistingData
org.h2.command.ddl.AlterTableAddConstraint.tryUpdate(AlterTableAddConstraint.java:282)
```

`AlterTableAddConstraint`'s own column check is
`DataType.areStableComparable(...)`, which **passes** for `INTEGER ARRAY` vs
`TIME ARRAY` on both VMs (that check is what raises
`UNCOMPARABLE_REFERENCED_COLUMN_2` for the neighbouring cases at lines 157 and
169, and those two *do* pass on CratonVM). The `TYPES_ARE_NOT_COMPARABLE_2` we
are missing is raised much later and incidentally: by
`ConstraintReferential.checkExistingData` building a `NOT EXISTS` probe query
and *preparing* it, at which point `Comparison.optimize` type-checks
`C."C" = P."C"`.

Everything that composition is made of agrees on both VMs:

| Component | HotSpot | CratonVM |
|---|---|---|
| `TypeInfo.checkComparable(INTEGER ARRAY, TIME ARRAY)` (direct call, reflectively built `TypeInfo`s) | throws | throws |
| `Value.GROUPS[]` (all 42 entries, reflectively read) | identical | identical |
| `SELECT CAST(NULL AS INT ARRAY) = CAST(NULL AS TIME ARRAY)` | throws 90110 | throws 90110 |
| the **exact** generated probe query, run through JDBC (see below) | throws 90110 | throws 90110 |
| `checkExisting` actually running for other shapes — a FK with violating rows, a `CHECK` with violating rows, and both with `NOCHECK` | runs / skipped as expected | identical |

The exact probe query, which throws on both:

```sql
SELECT 1 FROM (SELECT "C" FROM "PUBLIC"."B" WHERE "C" IS NOT NULL ORDER BY "C") C
  WHERE NOT EXISTS(SELECT 1 FROM "PUBLIC"."A" P WHERE C."C"=P."C")
```

So: the query text is right, the query throws when prepared through JDBC, the
type machinery is right, and `checkExisting` is not being suppressed wholesale —
yet the `ALTER TABLE` succeeds.

## What had not been checked — and why this list could not have worked

The gap is between `AlterTableAddConstraint.tryUpdate` line 282 and
`Comparison.optimize`. In descending order of suspicion:

1. **Is `ConstraintReferential.checkExistingData` entered at all for this
   statement?** A print at its first line, and at the `session.prepare(...)`
   call, settles it in one run. If it is not entered, `checkExisting` is false
   for this parse and the question moves into `Parser` (`readIf("NOCHECK")` and
   the `command.getType() != ALTER_TABLE_ADD_CONSTRAINT_PRIMARY_KEY` guard at
   `Parser.java:8812`).
2. **If it is entered: does `SessionLocal.prepare(String)` reach
   `Prepared.prepare()`?** The internal `session.prepare(sql)` is a different
   entry point from JDBC's `prepareCommand`, and the difference between "throws"
   and "does not throw" here is exactly whether the optimize pass runs. The
   probe above exercises the JDBC path, so it does not cover this.
3. **Is the `DbException` being thrown and swallowed?** `checkExistingData`
   wraps the query in `try (ResultInterface r = ...)` with a `finally`; a
   divergence in exception propagation through try-with-resources on the
   native/interpreted boundary would present exactly like this.

(1) and (2) are one instrumented build apart. This was not carried further in
this session because the other four clusters were the ones with fixes in them.

## Blast radius

A foreign key that H2 should refuse is created instead. The columns cannot
actually hold comparable values, so the constraint can never be satisfied by
non-null data — the practical effect is a schema that H2 accepts and later
misbehaves on, rather than silent data corruption. Narrow: it needs a FK between
two ARRAY columns whose element types are in different type groups.

## Repro

```bash
source /data/toolchain/env.sh
cd /data/cratonvm/apps/h2database/h2
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --nojit --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.scripts.TestScript
```

or, for the three-statement version, any JDBC program against
`jdbc:h2:mem:` with H2's `target/classes` on the classpath — the SQL is quoted
in full above.
