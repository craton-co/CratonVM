# H2 `ExpressionColumn.getValue` native override bypassed `SelectGroups` — every `GROUP BY` / `OVER(...)` row read the **last scanned source row**

**Status: FIXED** (`native-builtins/src/apps_h2.rs`, branch
`fix/hib-bulkid-insertselect-lastrow-20260727`).

One CratonVM defect. It was filed as **seven separate OPEN docs** in
`docs/known-issues/hibernate/`, several of which correctly suspected a single shared
substrate cause but none of which had root-caused it. This doc supersedes and closes
all of them.

## Root cause

`native-builtins/src/apps_h2.rs` registers a Rust override for
`org.h2.expression.ExpressionColumn.getValue(SessionLocal)` (registration at
`apps_h2.rs:223`, implementation `h2_expression_column_get_value`). It exists purely
as a throughput fast path: instead of running H2's method, it reads the column
straight off `TableFilter.current` / `TableFilter.currentSearchRow`.

H2's real method does something else first:

```java
public Value getValue(SessionLocal session) {
    Select select = columnResolver.getSelect();
    if (select != null) {
        SelectGroups groupData = select.getGroupDataIfCurrent(false);
        if (groupData != null) {
            Value v = (Value) groupData.getCurrentGroupExprData(this);
            if (v != null) return v;                       // <-- the whole ballgame
            if (select.isGroupWindowStage2()) throw DbException.get(MUST_GROUP_BY_1, ...);
        }
    }
    return columnResolver.getValue(column);                // <-- all the native did
}
```

That prologue is not an optimisation — it is **required for correctness** in grouped
and windowed queries. `Select.queryWindow`/`queryGroup` run in two phases:

1. `gatherGroup(...)` scans the source to completion. For every source row,
   `ExpressionColumn.updateAggregate` caches that row's column value into
   `SelectGroups.currentGroupByExprData` (per row for `Plain`/window, per group for
   `Grouped`).
2. `processGroupResult(...)` then **replays** the buffered rows and evaluates the
   select list, `HAVING`, and window `PARTITION BY` keys against them.

During phase 2 the scan is already over, so `TableFilter.current` is pinned at the
**last** source row. Reading the resolver directly — which is exactly what the native
did — therefore returned the last row's value for *every* emitted row.

That single behaviour produces every symptom the seven docs describe:

| observed symptom | mechanism |
|---|---|
| window query: all rows show the last source row's value | phase-2 read hits the live (last) row |
| `GROUP BY` non-key column: wrong group's value / one-group shift | ditto, per group |
| `HAVING col = ?` returns zero rows | predicate compares the last group's column |
| `PARTITION BY` key wrong → `HashMap` miss → `DbException("Window function")` | phase-2 `over.getCurrentKey()` is an `ExpressionColumn` |
| `PARTITION BY` key wrong → wrong hit → off-by-one / stuck-on-one-partition | same, but the wrong key exists |

Aggregates (`sum(...)`, `count(*)`) were always correct, because they accumulate
during phase 1 when the live row *is* the right row. That asymmetry is what made the
symptom read as "one column is wrong / one row is duplicated" rather than "the fast
path is semantically incomplete."

## Minimal, Hibernate-free reproduction

`apps/hib-suite-runner/H2WindowScanProbe.java` (plain JDBC + `h2-2.4.240.jar`,
no Hibernate) — before the fix:

```
                                                        HotSpot            CratonVM (broken)
select id, row_number() over() from Doctor            : 1/1 2/2 ... 10/10  10/1 10/2 ... 10/10
select (id+20), row_number() over() from Doctor       : 21/1 ... 30/10     30/1 30/2 ... 30/10
select (id+20), count(*) over() from Doctor           : 21/10 ... 30/10    30/10 x10
select id, sum(id) over() from Doctor                 : 1/55 ... 10/55     10/55 x10
select (id+20) from Doctor group by id                : 21 22 ... 30       22 23 ... 30 30
select id, lag(id) over(order by id) from Doctor      : 1/null 2/1 ...     10/null 10/1 ...
select sum(id) over() from Doctor                     : 55 x10             55 x10   (correct)
```

`apps/hib-suite-runner/BulkIdInsertSelectProbe.java` replays the exact SQL shape
Hibernate's `TableBasedInsertHandler` emits for a JOINED-inheritance bulk insert, and
showed the scratch table receiving **ten rows all keyed `30`** (not, as the original
`bulkid` doc assumed, one duplicated last row) with `rn_` correctly `1..10`.

After the fix both probes match HotSpot exactly on every shape.

## The fix

`h2_expression_column_get_value` now runs H2's own bytecode (via
`invoke_special_bytecode_only`, so it cannot re-enter the override) whenever the
prologue could matter:

* the resolver is not a `TableFilter` (other `ColumnResolver` shapes — notably
  `SelectListColumnResolver` — also carry a non-null `getSelect()`), **or**
* `TableFilter.select.groupData` is non-null, i.e. a grouped/windowed query is
  executing.

`groupData` is null for ordinary non-grouped queries, so the fast path — the reason
the native exists — is untouched there.

## Docs this closes

All seven were filed 2026-07-27 against
`apps/hib-suite-runner/runs/run-20260726-235842-passed/on-real/results.tsv`:

1. `bulkid-mutationstrategy-insertselect-lastrow-duplicate-20260727.md` (11 classes)
2. `size-groupby-aggregate-last-group-value-leak-20260727.md` (2 classes)
3. `windowfunction-partition-rowid-lookup-miss-shift-20260727.md` (3 classes)
4. `orderedsetaggregate-window-partition-value-staleness-20260727.md` (1 class)
5. `having-clause-and-conjunction-empty-result-20260727.md` (2 classes)
6. `dynamicinstantiation-groupby-join-varchar-column-row-swap-20260727.md` (1 class)
7. `parameterized-offset-ignored-groupby-having-20260727.md` (2 classes)

### Verification — all 21 classes, one run, fixed binary vs HotSpot

`apps/hib-suite-runner/bulkid-family.txt`, run as

```bash
cd C:/craton/CratonVM/apps/hib-suite-runner
# HotSpot baseline
"C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot/bin/java.exe" \
  @common.args CratonRunner bulkid-family.txt 0
# CratonVM, fixed binary
C:/craton/CratonVM-bulkid-insertselect-20260727/target/release/cratonvm-bulkid-h2group-20260727.exe \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" --Xmx 1500m \
  @common.args CratonRunner bulkid-family.txt 0
```

| doc | class | before | after | HotSpot |
|---|---|---:|---:|---:|
| 1 | `bulkid.DefaultMutationStrategyIdTest` | 5/6 | **6/6** | 6/6 |
| 1 | `bulkid.InlineMutationStrategyIdTest` | 5/6 | **6/6** | 6/6 |
| 1 | `bulkid.PersistentTableMutationStrategyIdTest` | 5/6 | **6/6** | 6/6 |
| 1 | `bulkid.GlobalTemporaryTableMutationStrategyIdTest` | 5/6 | **6/6** | 6/6 |
| 1 | `bulkid.LocalTemporaryTableMutationStrategyIdTest` | 5/6 | **6/6** | 6/6 |
| 1 | `bulkid.OracleInlineMutationStrategyIdTest` | 5/6 | **6/6** | 6/6 |
| 1 | `bulkid.DefaultMutationStrategyCompositeIdTest` | 4/5 | **5/5** | 5/5 |
| 1 | `bulkid.InlineMutationStrategyCompositeIdTest` | 4/5 | **5/5** | 5/5 |
| 1 | `bulkid.PersistentTableMutationStrategyCompositeIdTest` | 4/5 | **5/5** | 5/5 |
| 1 | `bulkid.GlobalTemporaryTableMutationStrategyCompositeIdTest` | 4/5 | **5/5** | 5/5 |
| 1 | `bulkid.LocalTemporaryTableMutationStrategyCompositeIdTest` | 4/5 | **5/5** | 5/5 |
| 2 | `query.hql.size.ManyToManySizeTest` | 3/9 | **9/9** | 9/9 |
| 2 | `query.hql.size.OneToManySizeTest` | 2/7 | **7/7** | 7/7 |
| 3 | `mapping.formula.FormulaWithPartitionByTest` | 0/1 | **1/1** | 1/1 |
| 3 | `query.criteria.CriteriaWindowFunctionTest` | 6/11 | **11/11** | 11/11 |
| 3 | `query.hql.WindowFunctionTest` | 2/7 | **7/7** | 7/7 |
| 4,7 | `query.criteria.CriteriaOrderedSetAggregateTest` | 7/10 | **10/10** | 10/10 |
| 7 | `query.hql.OrderedSetAggregateTest` | 7/8 | **8/8** | 8/8 |
| 5 | `query.criteria.CriteriaMultiselectGroupByAndOrderByTest` | 4/6 | **6/6** | 6/6 |
| 5 | `jpa.compliance.CriteriaFunctionParametersBindingTest` | 1/2 | **2/2** | 2/2 |
| 6 | `query.hql.instantiation.DynamicInstantiationWithJoinAndGroupAndOrderByByTest` | 0/1 | **1/1** | 1/1 |

**Total: 123/123 passing, zero failures — identical to HotSpot** (82/123 before the
fix; the per-class column above sums to 123, an earlier revision of this line said 118
by an arithmetic slip).

Doc 7 (`parameterized-offset-ignored-groupby-having`) was diagnosed as an
independent `OFFSET ? ROWS` binding bug. It is not: the query's
`HAVING eob2_0.id > 1` was itself mis-evaluating (the extra `id=2` row was a wrong
group's column reaching the predicate), and it goes green with this same fix.

## Throughput — same-commit A/B, no regression

The fix adds two by-name field reads (`TableFilter.select`, `Select.groupData`) to the
fast path. Measured with `apps/hib-suite-runner/H2QueryThroughputProbe.java`
(20k-row table, non-grouped predicate scan — i.e. the exact shape the native exists to
accelerate, and the shape that now pays the two extra reads), against a binary built
from the **identical tree with only `apps_h2.rs` reverted**, runs interleaved to cancel
host load:

| round | pre-fix | post-fix |
|---|---:|---:|
| 1 | 2252 ms | 2037 ms |
| 2 | 2203 ms | 2140 ms |

Within noise; no regression. The `invoke_virtual` into `Row.getValue(int)` dominates
this native, so two extra field lookups do not move it.

The other half of the trade-off is real but small: for grouped/windowed queries the
native now *delegates*, giving up the fast path entirely. Measured with
`H2GroupedThroughputProbe.java` over a 3300-row table, same A/B pair:

| query | pre-fix | post-fix |
|---|---:|---:|
| `select id, v, row_number() over() from T` | 1855 / 1821 ms | 1932 / 1901 ms (**+4 %**) |
| `select grp, max(id), max(v) from T group by grp` | 9 / 9 ms | 8 / 8 ms |
| `select id, rank() over(partition by grp order by v) from T` | **throws** | 3247 / 3234 ms |

~4 % on a large window query, nothing on `GROUP BY`. Note the pre-fix checksums for
the window query are *wrong* (`92540250` vs the correct `48993450`) — it was "fast"
partly because it kept re-reading one row — and the `PARTITION BY` case could not
execute at all pre-fix (H2's unreachable-by-design `"Feature not supported: Window
function"` fallback, symptom 1 of the `windowfunction-partition-rowid-lookup-miss-shift`
doc, reproduced here without Hibernate).

That pre-fix binary also serves as the A/B correctness control — it reproduces the
window and `GROUP BY` corruption exactly, on the same commit and toolchain, confirming
this change (and nothing else on `dev`) is what fixes it:

```
pre-fix : select (id+20), row_number() over() from Doctor -> 30/1 30/2 ... 30/10
pre-fix : select (id+20) from Doctor group by id          -> 22 23 ... 30 30
post-fix: both match HotSpot
```

## Residual (not this bug): `OracleInlineMutationStrategyIdTest` is timeout-marginal

Re-running the 21 classes after merging `origin/dev` (17 commits) on a heavily loaded
host, 20/21 were clean and this one reported `ok=5 failed=1` — **not** a wrong value:

```
run 1: TimeoutException: setUp(SessionFactoryScope) timed out after 120 seconds
run 2: TimeoutException: testDeleteFromPerson(...)  timed out after 120 seconds
```

A *different* method each run, i.e. wall-clock roulette against JUnit's 120 s
per-method limit — not a deterministic failure. Both runs contain **zero**
`ConstraintViolationException` / `key:3300`, and `testInsertSelect` (this doc's actual
defect) passes in both.

It is the heaviest class in the set: `@BeforeEach` persists 6600 rows one at a time,
six times over (HotSpot runs the whole class in 5.3 s; CratonVM needs ~15 min), so
every method sits near the limit. It passed **6/6** on the pre-merge build with this
same fix (864 s) on a quieter box; the failing runs were 922–1007 s while peer VMs on
this shared host were burning thousands of CPU-seconds.

Attribution: **not caused by this change.** The class's cost is 6600 plain single-row
inserts with no grouped or windowed query in the fixture, and the measurements above
put this change at +4 % on window queries and 0 % elsewhere — nowhere near enough to
move a 15-minute class. It belongs to the general interpreter-throughput bucket, not
here.

## Other verification

- **Regression sample:** 30 previously-passing `query.criteria` / `query.hql` classes —
  zero failures, zero aborts.
- **`--nojit`:** `PersistentTableMutationStrategyIdTest`, `WindowFunctionTest`,
  `ManyToManySizeTest`, `CriteriaMultiselectGroupByAndOrderByTest` = 28/28. The
  original defect reproduced under `--nojit`, so this matters.

## Lesson

A native "fast path" that re-implements part of a library method silently inherits
responsibility for **all** of that method's semantics, including branches that look
like they belong to an unrelated feature. The `bulkid` doc's own conclusion — "H2 runs
as ordinary bytecode on top of CratonVM, so the defect is most likely a generic
duplicate-iteration bug in that shared substrate, not something specific to JDBC" —
was exactly backwards: CratonVM *does* intercept H2 here. Before assuming a
third-party library runs unmodified, grep `native-builtins/` for its package name.
