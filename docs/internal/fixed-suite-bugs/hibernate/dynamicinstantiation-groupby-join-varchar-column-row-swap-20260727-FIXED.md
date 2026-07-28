# `DynamicInstantiationWithJoinAndGroupAndOrderByByTest` — one `VARCHAR` column in a grouped join query is extracted with the *other* row's value

> **RESOLVED 2026-07-27 — this doc is retired.** Root cause: CratonVM's Rust
> override of `org.h2.expression.ExpressionColumn.getValue`
> (`native-builtins/src/apps_h2.rs`) skipped H2's `SelectGroups` prologue, so in a
> grouped/windowed query every emitted row read the **last scanned source row**
> instead of its own buffered value. Fixed by delegating to H2's own bytecode
> whenever `TableFilter.select.groupData` is non-null (or the resolver is not a
> `TableFilter`). All classes in this doc now pass, matching HotSpot exactly.
> Full analysis, the Hibernate-free repro, and the 21-class verification table:
> [`h2-expressioncolumn-getvalue-native-bypasses-groupdata-20260727-FIXED.md`](h2-expressioncolumn-getvalue-native-bypasses-groupdata-20260727-FIXED.md).
> The historical investigation below is preserved as written; note that its own
> root-cause speculation (a generic iteration/GC defect in shared substrate) was
> wrong — CratonVM does intercept H2 at this call.

**Status: OPEN, genuine CratonVM bug.** Confirmed via HotSpot diff (fails on CratonVM,
100% clean on real HotSpot JDK 25, byte-identical SQL text both sides), confirmed NOT a
suite-subset/ordering artifact (reproduces in a fresh single-class process, alone,
self-contained `@BeforeAll` fixture).

## Scope

`org.hibernate.orm.test.query.hql.instantiation
.DynamicInstantiationWithJoinAndGroupAndOrderByByTest::testInstantiationGroupByAndOrderBy`
— the class's only `@Test`, `found=1 ok=0 failed=1`.

Source: `apps/hib-suite-runner/runs/run-20260726-235842-passed/on-real/results.tsv`
(binary: worktree `CratonVM-hib-local-0712` merged with `origin/dev` @ `13055f75c`, real-JDK, JIT on).

## Symptom

HQL: `select new ...Summary(i, sum(is.total)) from ItemSale is join is.item i group by i
order by i`, generating:

```
select
    i1_0.id,
    i1_0.name,
    sum(is1_0.total)
from ItemSale is1_0
join Item i1_0 on i1_0.id=is1_0.item_id
group by i1_0.id
order by i1_0.id
```

(2 fixture rows: `Item 1` has sales totalling 3.0, `Item 2` has sales totalling 5.0 —
identical SQL text on both sides.)

| column | HotSpot row 1 | HotSpot row 2 | CratonVM row 1 | CratonVM row 2 |
|---|---|---|---|---|
| `id` (BIGINT) | `1` | `2` | `1` (correct) | `2` (correct) |
| `name` (VARCHAR) | `Item 1` | `Item 2` | **`Item 2`** (wrong) | `Item 2` (correct) |
| `sum` (DOUBLE) | `3.0` | `5.0` | `3.0` (correct) | `5.0` (correct) |

The `id` and `sum` columns are extracted correctly for both rows. Only the `name`
(`VARCHAR`) column is wrong, and only on the first row — it reads back the *second* row's
value (`Item 2`) instead of its own (`Item 1`). This manifests as:

```
org.opentest4j.AssertionFailedError: expected: <Item 1> but was: <Item 2>
```
at `assertEquals("Item 1", resultList.get(0).getItem().getName())`.

## Repro (isolated, fresh process)

```bash
cd C:/craton/CratonVM/apps/hib-suite-runner
printf "org.hibernate.orm.test.query.hql.instantiation.DynamicInstantiationWithJoinAndGroupAndOrderByByTest\n" > /tmp/single-dynaminst.txt

# CratonVM:
"C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" --Xmx 1500m \
  @common.args -Dcraton.batch=1 -Dcraton.trace=1 -Dhibernate.show_sql=true \
  CratonRunner /tmp/single-dynaminst.txt 0
# -> found=1 ok=0 failed=1 -- expected: <Item 1> but was: <Item 2>

# Real HotSpot JDK 25, identical harness/classpath/H2:
"C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot/bin/java.exe" \
  -Xmx1500m @common.args -Dcraton.batch=1 -Dhibernate.show_sql=true \
  CratonRunner /tmp/single-dynaminst.txt 0
# -> found=1 ok=1 failed=0  (clean)
```

## Analysis

Since the SQL text is identical both sides and H2 is the real, unmodified driver, the
`id`/`sum` columns for both rows and the `name` column for row 2 are all read correctly —
only `name` for row **1** is wrong, and it happens to equal what row 2's `name` should be.
This points at a per-column (not per-row, not per-statement) extraction defect specific
to this `VARCHAR` column position in this dynamic-instantiation + join + group-by shape:
Hibernate hydrates the joined `Item` entity from columns 1–2 of the `ResultSet` as part of
building each `Summary` DTO via the constructor-based instantiation
(`new Summary(i, sum(is.total))`), which is a different code path from a plain scalar
projection. The wrong value looks like a one-row-ahead read of the `name` column
specifically — as if the extraction for that column on row 1 fetched (or cached) the
column value that would/will be seen on row 2, while every other column on every other row
read correctly in row-order. This is a distinct symptom from the "stuck-on-first-row"
window-partition bug in
[`orderedsetaggregate-window-partition-value-staleness-20260727.md`](orderedsetaggregate-window-partition-value-staleness-20260727.md)
(there, *every* row repeats the *first* computed value; here, only *row 1*'s `name` picks
up *row 2*'s value) — kept as an independent bug rather than merged, though both fall in
the broad "wrong row's scalar value read back during multi-row extraction" family and are
worth cross-checking if either gets root-caused.

Also related:
[`size-groupby-aggregate-last-group-value-leak-20260727.md`](size-groupby-aggregate-last-group-value-leak-20260727.md)
— another `GROUP BY` query where the wrong value is read back, but there *every* group's
select-list aggregate collapses to the *last* group's true value rather than one column
of one row swapping with another.

## Next steps (not done here)

1. Minimal repro: run the same 2-column-plus-aggregate `GROUP BY ... JOIN` query via
   plain JDBC (no Hibernate dynamic-instantiation) to check whether the `VARCHAR` column
   read is wrong even without entity hydration — this would show whether the defect is in
   generic `ResultSet.getString` extraction under CratonVM or specific to Hibernate's
   join-entity hydration path used for constructor-based DTO projection.
2. If it only reproduces via Hibernate's dynamic-instantiation/join-entity hydration
   path, look at whichever code pre-fetches/caches entity-identifier-adjacent columns
   (id/name) ahead of the row cursor for join-fetch construction.
3. Once fixed, re-run this class — expect `ok=1/1`.

## Repro artifacts

Single-class listfile only (`/tmp/single-dynaminst.txt` per the repro commands above); no
other scratch files were created or need to be committed.
