# `OVER(...)` window functions — partition/row-id correlation is broken: missed lookups throw H2's "unreachable" fallback, hit lookups can be off-by-one

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
>
> **2026-09-06 note.** `CriteriaWindowFunctionTest` (this doc's own class,
> previously verified 11/11) showed 2 failures again in a 2026-09-06 suite run —
> `#testCountAsWindowFunctionWithFilter` and `#testNthValue`, both
> `expected: <5> but was: <1>` (a wrong **row count**, not a wrong value or the
> "unreachable" fallback exception this doc describes). This is a different,
> newly discovered JIT-warm-up-dependent defect, not a regression of the
> `groupData` delegation fix — it only reproduces after other tests warm up the
> JIT in the same process, never in a fresh single-method process or under
> `--nojit`. See
> `docs/internal/fixed-bugs/jit-warm-groupdata-window-row-collapse-20260906-FIXED.md`.

**Status: OPEN, genuine CratonVM bug.** Confirmed via HotSpot diff (fails on CratonVM,
100% clean on real HotSpot JDK 25, byte-identical SQL text and bound parameters both
sides, same H2 2.4.240 jar). NOT an H2-dialect-capability/environmental gap — see
"Ruled out: H2 dialect capability" below.

## Scope

3 classes, all exercising ANSI SQL window functions (`OVER (PARTITION BY ... ORDER BY
...)`, `rank()`, `row_number()`, `dense_rank()`, filtered/framed aggregates over a
window) via HQL or Criteria:

| Class | found/ok (CratonVM) | found/ok (HotSpot) |
|---|---:|---:|
| `org.hibernate.orm.test.mapping.formula.FormulaWithPartitionByTest` | 1/0 | 1/1 |
| `org.hibernate.orm.test.query.criteria.CriteriaWindowFunctionTest` | 11/6 | 11/11 |
| `org.hibernate.orm.test.query.hql.WindowFunctionTest` | 7/2 | 7/7 |

Source: `apps/hib-suite-runner/runs/run-20260726-235842-passed/on-real/results.tsv`
(binary: worktree `CratonVM-hib-local-0712` merged with `origin/dev` @ `13055f75c`,
real-JDK, JIT on). Re-confirmed with `-Dcraton.trace=1` for full stack traces and with
a direct head-to-head run of all 5 target classes (this doc's 3 + the `size()` pair
documented separately) against real HotSpot JDK 25 using the identical
`CratonRunner`/`common.args` harness — CratonVM: 3/24 total across the 3 classes;
HotSpot: 24/24 clean, zero failures, same run.

## Ruled out: H2 dialect capability / feature-detection

Both `FormulaWithPartitionByTest` (`@RequiresDialectFeature(feature =
DialectFeatureChecks.SupportPartitionBy.class)`) and most of `WindowFunctionTest`'s
methods (`@RequiresDialectFeature(feature =
DialectFeatureChecks.SupportsWindowFunctions.class)`) are gated by a dialect-capability
check. That check is a **static** capability flag on `H2Dialect`, not a live
runtime probe, and it correctly evaluates `true` for H2 — which is factually correct:
real H2 2.4.240 (the exact jar this suite depends on, `apps/hibernate-orm/gradle/
libs.versions.toml:75`) does support window functions, confirmed conclusively by the
HotSpot run above where every one of these window-function queries executes and
returns correct results. So the tests are correctly **not** skipped, and the
dialect-feature-detection path itself is not implicated — the defect is downstream of
that, in actual query execution.

## Symptom 1: `GenericJDBCException: Feature not supported: "Window function"`

This looks like an environmental H2 limitation but is not one — it is H2's own
internal "this should be unreachable" fallback, tripped by a CratonVM-side execution
bug, not H2 declining to support windowing.

```
select di1_0.DISPLAY_ITEM_ID,di1_0.DISCOUNT_CODE,di1_0.DISCOUNT_VALUE,
    ROW_NUMBER() OVER(PARTITION BY di1_0.DISCOUNT_CODE ORDER BY di1_0.DISPLAY_ITEM_ID)
from DisplayItem di1_0 order by di1_0.DISPLAY_ITEM_ID
```

fails on CratonVM with:

```
org.hibernate.exception.GenericJDBCException: JDBC exception executing SQL
[Feature not supported: "Window function"; ...]
Caused by: org.h2.jdbc.JdbcSQLFeatureNotSupportedException: Feature not supported: "Window function"
	at org.h2.message.DbException.getUnsupportedException(DbException.java:287)
	at org.h2.expression.analysis.WindowFunction.getAggregatedValue(WindowFunction.java:417)
	at org.h2.expression.analysis.DataAnalysisOperation.getWindowResult(DataAnalysisOperation.java:424)
	at org.h2.expression.analysis.DataAnalysisOperation.getValue(DataAnalysisOperation.java:394)
	at org.h2.command.query.Select.constructGroupResultRow(Select.java:569)
	at org.h2.command.query.Select.processGroupResult(Select.java:536)
	at org.h2.command.query.Select.queryWindow(Select.java:445)
```

on **real HotSpot the identical query, identical H2 jar, executes correctly** (`ok=1
failed=0`). The exact same query (with a `GROUP BY` appended by a different test
method) also succeeds on CratonVM — so it is not "H2 doesn't support `PARTITION BY`",
it is one specific code path inside H2's own window-evaluation machinery.

Decompiling the real, unmodified `h2-2.4.240.jar`
(`~/.gradle/caches/modules-2/files-2.1/com.h2database/h2/2.4.240/.../h2-2.4.240.jar`,
via `javap -c -p`) shows `DataAnalysisOperation.getWindowResult` does, in Java-source
terms:

```java
private Value getWindowResult(SessionLocal session, SelectGroups groupData) {
    boolean ordered = over.isOrdered();
    Value key = over.getCurrentKey(session);
    PartitionData partition = groupData.getWindowExprData(this, key);
    Object data = (partition == null) ? (ordered ? new ArrayList<>() : createAggregateData())
                                       : partition.getData();
    if (partition == null) groupData.setWindowExprData(this, key, partition = new PartitionData(data));
    if (ordered || isAggregate()) {
        Value result = getOrderedResult(session, groupData, partition, data);
        return result != null ? result : getAggregatedValue(session, null); // <-- unreachable-by-design fallback
    }
    ...
}
```

and `getOrderedResult` (also in `DataAnalysisOperation`) builds — once per partition,
lazily — a `HashMap<Integer, Value>` mapping every row's `getCurrentGroupRowId()` to
its computed rank/row-number/etc. (via the abstract `getOrderedResultLoop`, which
H2's `WindowFunction` implements with a `tableswitch` over `WindowFunctionType`), then
does a single `map.get(currentGroupRowId)` to return this row's value. `WindowFunction`
overrides `createAggregateData()`/`getAggregatedValue()` to unconditionally `throw
DbException.getUnsupportedException("Window function")` — those two methods exist
purely as an assertion that "a true window function should never need the plain-
aggregate code path"; they are unreachable in correct H2 operation. On CratonVM they
**are** reached, which only happens if `getOrderedResult`'s `HashMap.get(rowId)`
misses (returns `null`) for a row that the loop should have already populated — i.e.
the row-id used to populate the map during the collection pass and the row-id used to
look a value up for the current row during emission disagree for that partition/query
shape. That is a per-row/per-partition **correlation** bug in CratonVM's execution of
this (real, unmodified) H2 bytecode, not an H2 capability gap.

## Symptom 2: wrong values (assertion failures, not exceptions)

`CriteriaWindowFunctionTest` (2 of its 5 failures) and `WindowFunctionTest` both show
window-function queries that execute *without* the exception above but return the
*wrong* value — same underlying HashMap-lookup mechanism, but a **hit** on the wrong
entry instead of a miss:

- `CriteriaWindowFunctionTest`: `expected: <1> but was: <2>`, `expected: <null> but
  was: <6>`.
- `WindowFunctionTest::testSumWithFilterAsWindowFunction` — `select sum(eob.theInt)
  filter (where eob.theInt > 5) over (order by eob.theInt) from EntityOfBasics eob
  order by eob.theInt`, 5 rows. Correct sequence (HotSpot): `null, null, 6, 13, 26`.
  CratonVM's `ResultSet` extraction order: `null, 6, 13, 26, null`. This is exactly
  `output[i] = correct[(i+1) mod 5]` — a clean off-by-one row-id shift **with
  wraparound** (the last row picks up the first row's value instead of running off
  the end), pointing at a fencepost error in how the row-id counter used to key the
  `HashMap` during collection vs. lookup is incremented/reset.

## Symptom 3: empty result list (no exception, no wrong value — zero rows)

`WindowFunctionTest::testRank`, `::testRowNumberWithoutOrder`, and `::testFrame` all
fail their very first assertion, `assertEquals(5, resultList.size())`, with `expected:
<5> but was: <0>`. The raw log confirms **zero** `extracted value` lines follow the
`Hibernate:` SQL log line for these queries (e.g. `select rank() over(partition by
eob1_0.the_int order by eob1_0.id) from EntityOfBasics eob1_0 order by 1`) — the
query executes without error but the JDBC `ResultSet` yields no rows at all on
CratonVM, where HotSpot returns the correct 5. `WindowFunctionTest::testOrderByAndAlias`
fails with `jakarta.persistence.NoResultException` for the same underlying reason: it
looks up a specific `dense_rank()` value inside a derived table, and that rank value
is never present in CratonVM's (broken) output.

## Repro (isolated, fresh process; also demonstrates the HotSpot A/B)

```bash
cd C:/craton/CratonVM/apps/hib-suite-runner
printf "org.hibernate.orm.test.mapping.formula.FormulaWithPartitionByTest\norg.hibernate.orm.test.query.criteria.CriteriaWindowFunctionTest\norg.hibernate.orm.test.query.hql.WindowFunctionTest\n" > /tmp/single-window.txt

# CratonVM, fresh process, 3 classes, alone:
"C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" --Xmx 1500m \
  @common.args -Dcraton.batch=3 -Dcraton.trace=1 CratonRunner /tmp/single-window.txt 0
# -> FormulaWithPartitionByTest found=1 ok=0 failed=1
# -> CriteriaWindowFunctionTest found=11 ok=6 failed=5
# -> WindowFunctionTest found=7 ok=2 failed=5

# Same classes, real HotSpot JDK 25, identical harness/classpath/H2:
"C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot/bin/java.exe" \
  @common.args -Dcraton.batch=3 CratonRunner /tmp/single-window.txt 0
# -> FormulaWithPartitionByTest found=1 ok=1 failed=0
# -> CriteriaWindowFunctionTest found=11 ok=11 failed=0
# -> WindowFunctionTest found=7 ok=7 failed=0   (100% clean, all 3 classes)
```

## Analysis

H2 is the real, unmodified JDBC driver running as ordinary application bytecode on
top of CratonVM — CratonVM does not implement or intercept SQL execution. The exact
same SQL, same bound parameters, same H2 jar produces correct results on HotSpot, so
the query translation (Hibernate → SQL) is not the bug. The defect is downstream, in
how CratonVM's interpreter/runtime executes H2's own window-function row-buffering:
specifically, the correlation between "the row-id a computed window value was stored
under" and "the row-id used to look that value up again for the current output row" —
tracked through H2's own `HashMap<Integer, Value>`/`SelectGroups` per-partition state —
loses sync under CratonVM. Depending on the exact query shape this shows up as a
missed lookup (→ H2's unreachable-by-design exception fallback), an off-by-one hit
with wraparound (→ wrong value), or total loss of all row correlations for that
partition (→ empty result set).

This is very likely the same general *substrate* defect already tracked as OPEN in
this directory under two other symptom shapes:

- [`bulkid-mutationstrategy-insertselect-lastrow-duplicate-20260727.md`](bulkid-mutationstrategy-insertselect-lastrow-duplicate-20260727.md)
  — `row_number() over()` in a bulk `INSERT ... SELECT`, last row duplicated.
- [`orderedsetaggregate-window-partition-value-staleness-20260727.md`](orderedsetaggregate-window-partition-value-staleness-20260727.md)
  — ordered-set aggregates (`percentile_disc`/`listagg`) used `OVER (PARTITION BY
  ...)`, every row stuck on the *first* partition's value.

All four docs (this one plus the two above) involve H2's `OVER(...)` window-function
row/partition buffering producing wrong per-row correlation under CratonVM, with
different concrete symptoms (duplicated last row; stuck-on-first-partition; missed
HashMap lookup → exception; off-by-one-with-wraparound; empty result set) that have
not been merged into one doc because none is yet root-caused to a specific source
line — but this doc adds three new classes, a bytecode-level trace of the exact H2
call chain responsible for the "Feature not supported" symptom (previously
undocumented and easy to misdiagnose as an environmental H2-capability gap), and a
precise `(i+1) mod N` characterization of the off-by-one shift. Any future
investigation into this substrate bug should re-verify against all four docs.

## Next steps (not done here)

1. Write a minimal non-Hibernate H2 repro (`jdbc:h2:mem:`) with a plain
   `SELECT ... OVER (PARTITION BY ...)` / `ROW_NUMBER() OVER()` query with no
   Hibernate/Criteria involved, to confirm the missed-lookup and off-by-one-shift
   behavior reproduces at the plain-JDBC level (this would settle whether the defect
   is generic HashMap/collection-iteration under CratonVM's interpreter/JIT, or
   something specific to how Hibernate drives H2).
2. If confirmed, bisect `--nojit` vs JIT-on (not yet tested for this doc's 3 classes)
   to rule in/out the JIT — the sibling `bulkid` doc found its symptom is
   **not** JIT-specific (reproduces under `--nojit`), which is a useful prior for
   where to look first (interpreter/collections/GC substrate, not JIT codegen).
3. Once root-caused, re-run all 3 classes here plus the 2 in the `bulkid` doc and
   the 1 in `orderedsetaggregate` doc together to confirm one fix closes all of them.

## Repro artifacts

Single-listfile repro only (`/tmp/single-window.txt` per the commands above); no
other scratch files were created or need to be committed.
