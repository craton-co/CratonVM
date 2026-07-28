# `testHypotheticalSetRankWithGroupByHavingOrderByLimit` (Criteria + HQL) — parameterized `OFFSET ? ROWS` silently not applied on a grouped/ranked query

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
100% clean on real HotSpot JDK 25, byte-identical SQL text and identically-bound `?`
parameter both sides), confirmed NOT a suite-subset/ordering artifact (reproduces in a
fresh single-class process, alone).

## Scope

The exact same query, generated two different ways, both fail identically:

- `org.hibernate.orm.test.query.criteria.CriteriaOrderedSetAggregateTest
  ::testHypotheticalSetRankWithGroupByHavingOrderByLimit`
- `org.hibernate.orm.test.query.hql.OrderedSetAggregateTest
  ::testHypotheticalSetRankWithGroupByHavingOrderByLimit` (HQL form of the same query;
  this is the *only* failure in this class — `found=8 ok=7 failed=1`)

Source: `apps/hib-suite-runner/runs/run-20260726-235842-passed/on-real/results.tsv`
(binary: worktree `CratonVM-hib-local-0712` merged with `origin/dev` @ `13055f75c`, real-JDK, JIT on).

## Symptom

```
select
    eob2_0.id c0,
    rank(5) within group (order by eob1_0.the_int) c1
from EntityOfBasics eob1_0
cross join EntityOfBasics eob2_0
group by c0
having eob2_0.id>1
order by 1, 2
offset ? rows
```
bind: `(1:INTEGER) <- [1]` — identical on both sides.

| | HotSpot (correct) | CratonVM (wrong) |
|---|---|---|
| extracted `id` values | `3, 4, 5` (3 rows) | `2, 3, 4, 5` (4 rows) |
| test assertion | `assertEquals(3, resultList.size())` passes | `expected: <3> but was: <4>` |

The `HAVING eob2_0.id > 1` filter itself is evaluated **correctly** on both sides (both
produce the 4 candidate groups for ids 2,3,4,5 before any offset is applied — confirmed
by HotSpot's own pre-offset group count matching CratonVM's post-having row count). The
divergence is specifically that CratonVM's `OFFSET ? ROWS` clause, with the parameter
correctly bound to `1`, does not skip the first row of the ordered result — CratonVM
returns the row for `id=2` (which HotSpot correctly skips) *in addition to* `3, 4, 5`.

## Repro (isolated, fresh process)

```bash
cd C:/craton/CratonVM/apps/hib-suite-runner
printf "org.hibernate.orm.test.query.hql.OrderedSetAggregateTest\n" > /tmp/single-osa-hql.txt

# CratonVM:
"C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" --Xmx 1500m \
  @common.args -Dcraton.batch=1 -Dcraton.trace=1 -Dhibernate.show_sql=true \
  CratonRunner /tmp/single-osa-hql.txt 0
# -> found=8 ok=7 failed=1 -- expected: <3> but was: <4>

# Real HotSpot JDK 25, identical harness/classpath/H2:
"C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot/bin/java.exe" \
  -Xmx1500m @common.args -Dcraton.batch=1 -Dhibernate.show_sql=true \
  CratonRunner /tmp/single-osa-hql.txt 0
# -> found=8 ok=8 failed=0  (100% clean)
```

The Criteria-API twin (`CriteriaOrderedSetAggregateTest`) reproduces the identical SQL
and identical wrong row count, confirming this is a query-shape-triggered bug, not
something specific to either the Criteria builder or the HQL parser — both translate to
the same SQL AST and hit the same execution-time defect.

## Analysis

Since the SQL text and the bound offset parameter value are identical between CratonVM
and HotSpot, and H2 is the real, unmodified driver, this is not a SQL-generation/SQM
translation bug — it's an execution-time defect in how CratonVM (running H2's own
bytecode) handles a **parameterized** `OFFSET ? ROWS` clause specifically in combination
with `GROUP BY ... HAVING ... ORDER BY`. Other tests in the same classes that use `OFFSET`
without `GROUP BY`/`HAVING` were not observed to fail in this run, so the trigger appears
narrower than "OFFSET is broken everywhere" — it is at minimum broken for this
grouped+having+ranked shape, and worth checking against a plain (non-grouped) parameterized
offset query to see how narrow the trigger really is.

## Next steps (not done here)

1. Minimal repro: `SELECT id FROM t ORDER BY id OFFSET ? ROWS` (no GROUP BY/HAVING) with a
   bound parameter, to determine whether parameterized OFFSET is broken universally or
   only when combined with GROUP BY/HAVING/window-ish aggregates.
2. If narrow, suspect the offset/limit clause is being computed or applied against the
   pre-aggregation row count (25 cross-joined rows) rather than the post-`GROUP
   BY`/`HAVING` row count (4 groups), or that the offset is applied then the first
   already-skipped row is being re-added by whatever produces the final grouped output.
3. Once fixed, re-run both classes above — expect `ok=8/10` for
   `CriteriaOrderedSetAggregateTest` (2 other failures are the separate window-partition
   bug, tracked in
   [`orderedsetaggregate-window-partition-value-staleness-20260727.md`](orderedsetaggregate-window-partition-value-staleness-20260727.md))
   and `ok=8/8` for `OrderedSetAggregateTest`.

## Repro artifacts

Single-class listfiles only (`/tmp/single-osa-hql.txt` per the repro commands above); no
other scratch files were created or need to be committed.
