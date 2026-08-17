# `TestScript` — a foreign key between uncomparable ARRAY types was accepted: the H2 `checkExistingData` native skipped the type check along with the row scan

## Status
**FIXED 2026-08-17**, on branch `fix/h2-testscript-fk-comparability-20260817` off
`dev`, worktree `/data/cvm-h2sql-20260816`, Azure host `azureuser@20.80.105.49`.
Verified by a same-session differential against real HotSpot JDK 25 on the same
host, same JDK image, same classpath.

Supersedes the OPEN record
`docs/known-issues/h2/testscript-foreign-key-existing-data-check-not-run-20260816.md`,
which had this isolated to a three-statement repro but explicitly not
root-caused. Its three ranked hypotheses were all wrong, in an instructive way —
see "Why the Java source was a dead end".

## The failure

```
ERROR: org/h2/test/scripts/ddl/alterTableAdd.sql
line: 166
exp: > exception TYPES_ARE_NOT_COMPARABLE_2
got: > ok
```

Three statements reproduce it with no other DDL:

```sql
CREATE TABLE A(A TIMESTAMP PRIMARY KEY, B INT ARRAY UNIQUE, C TIME ARRAY UNIQUE);
CREATE TABLE B(A TIMESTAMP WITH TIME ZONE, B DATE, C INT ARRAY, D TIME ARRAY, E TIME WITH TIME ZONE ARRAY);
ALTER TABLE B ADD FOREIGN KEY(C) REFERENCES A(C);   -- INTEGER ARRAY -> TIME ARRAY
```

HotSpot refuses it with `90110`. CratonVM created the constraint.

## Root cause

`native-builtins/src/apps_h2.rs` registers a native override for
`org.h2.constraint.ConstraintReferential.checkExistingData(SessionLocal)`
(gated JIT-side by `vm/src/runtime/interpreter/native_override.rs:954`). It was
added by `e48e14cc9` *"Fix Hibernate FunctionTests hot paths"* — Hibernate
creates a lot of foreign keys against freshly created, empty tables, and the
native short-circuits that:

```rust
if matches!(
    ctx.invoke_virtual(table, "getRowCount", "(Lorg/h2/engine/SessionLocal;)J", &[...])?,
    Some(Value::Long(0))
) {
    return Ok(None);                       // nothing to validate, skip the query
}
h2_constraint_run_existing_data_query(ctx, this, session)
```

The premise — "no rows, nothing to validate" — is false, because H2's method does
two things and only one of them is about rows. It builds a probe query and
**prepares** it:

```java
try (ResultInterface r = session.prepare(builder.toString()).query(1)) { ... }
```

and preparing `... WHERE C."C"=P."C"` runs `Comparison.optimize`, which calls
`TypeInfo.checkComparable(leftType, rightType)` and raises
`TYPES_ARE_NOT_COMPARABLE_2`. That type check is a *side effect of preparing*,
it has nothing to do with how many rows exist, and H2 raises it on an empty
table exactly as on a full one. Skipping the prepare skipped the type check with
it.

Note the shortcut is not wrong about the scan — it is wrong about what else was
riding along on the code path it removed.

This also explains the confusing behaviour that made the first pass give up:
`ALTER TABLE F ADD FOREIGN KEY(ID) REFERENCES P(ID)` against a table **with**
violating rows correctly threw `REFERENTIAL_INTEGRITY_VIOLATED_PARENT_MISSING_1`
on CratonVM, so "the check runs" looked established. It did run — the row count
was non-zero, so the native fell through to the real query. Only the empty-table
path was affected, and `alterTableAdd.sql` happens to test exactly that.

## Why the Java source was a dead end

Every one of the OPEN record's three next-step hypotheses (is
`checkExistingData` entered; does `SessionLocal.prepare` reach
`Prepared.prepare`; is the exception swallowed) assumed the Java body runs.
`native_override.rs` shadows the method by **name + descriptor**, so:

* the class was demonstrably the instrumented one — `INST_MARKER` field readable,
  `getProtectionDomain().getCodeSource()` pointing at the instrumented
  directory, `getDeclaredMethods()` listing exactly one
  `checkExistingData(SessionLocal)`;
* reflection resolved the method to `ConstraintReferential`;
* and calling it — through the exact type, through the `Constraint` base type,
  and through `Method.invoke` — ran **none** of the body. Replacing the entire
  body with a bare `throw` changed nothing: the call still returned normally.

A generic sibling-override dispatch probe (abstract base, four subclasses, one
with an empty body, in H2's exact shape) matched HotSpot perfectly, ruling out a
dispatch bug and pointing at something keyed to this one method. `grep -rn
checkExistingData --include=*.rs` then found it in two lines.

**The lesson worth carrying: when instrumenting an application's Java source
under CratonVM produces impossible results — a body that provably does not run —
check `native_override.rs` and `apps_h2.rs` (and their per-framework siblings)
before doubting the VM's dispatch.** An overridden method's Java source is not
what executes.

## The fix

Hoist the type check out of the shortcut, so the fast path only skips what is
genuinely a no-op on an empty table — scanning it for orphaned rows:

```rust
h2_constraint_check_column_types(ctx, this)?;      // new: always
// ... unchanged: isStarting guard above, row-count shortcut below
```

`h2_constraint_check_column_types` calls
`TypeInfo.checkComparable(columns[i].getType(), refColumns[i].getType())` for
each column pair — precisely the pairs the generated query would have compared,
and precisely the check preparing it would have performed. O(columns) instead of
a parse-and-optimize, so the Hibernate fast path keeps its win.

`Column.getType()` is a plain field getter and cannot allocate, so no moving GC
can run between reading the two `TypeInfo`s and passing them as arguments; the
helper needs no pinning, and says so.

## Verification

```
                                                       HotSpot    CratonVM before    CratonVM after
ALTER TABLE B ADD FOREIGN KEY(C) REFERENCES A(C)        90110      accepted           90110
  (empty tables, the failing case)
ALTER TABLE F ADD FOREIGN KEY(ID) REFERENCES P(ID)      23506      23506              23506
  (violating rows present — must still fire)
... NOCHECK variants                                    ok         ok                 ok
ALTER TABLE G ADD CONSTRAINT ... CHECK(...)             23513      23513              23513
```

`org.h2.test.scripts.TestScript`, same classpath: **10 errors → 9**, the one
that went green being `ddl/alterTableAdd.sql:166`. HotSpot remains at 0.

The 9 that remain are two root causes, both recorded:
[`../../../known-issues/h2/testscript-concurrenthashmap-iteration-order-20260816.md`](../../../known-issues/h2/testscript-concurrenthashmap-iteration-order-20260816.md)
(4) and
[`../../../known-issues/h2/testscript-collation-turkish-and-locale-display-names-20260816.md`](../../../known-issues/h2/testscript-collation-turkish-and-locale-display-names-20260816.md)
(5).

## Residual

The native reimplements `checkExistingData` rather than deferring to it, so any
*other* behaviour H2 gets from preparing the probe query is still absent. The
column-type check was the one with an observable consequence in this suite;
identifier resolution cannot fail here, since the query is built from already
resolved `Column` objects. If a third rider on `prepare` ever shows up, the
honest fix is to stop reimplementing the method and instead prepare always,
skipping only `query(1)`/`next()` when the row count is zero.

## Repro (against a binary without the fix)

```bash
source /data/toolchain/env.sh
cd /data/cratonvm/apps/h2database/h2
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --nojit --Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.scripts.TestScript
```

or the three statements above through JDBC against `jdbc:h2:mem:` with H2's
`target/classes` on the classpath.
