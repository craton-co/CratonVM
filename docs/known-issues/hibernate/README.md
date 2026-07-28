# Hibernate ORM suite — open known issues

## Open

- [`action.queue` GRAPH-default tests — blocked by flush-planner throughput](actionqueue-graph-default-tests-legacy-tradeoff-20260727.md)
  (OPEN; one of two root causes fixed) — real-JDK CratonVM defaults
  `hibernate.flush.queue.type` to `legacy`, which gates **19 of the 25
  `action.queue` classes**: 2 fail outright and 17 self-abort via
  `Assumptions.abort("Skipping GRAPH test with non-GRAPH queue type")` (the
  earlier WON'T-FIX doc reported only the 2). Root cause 1 — records
  (`GroupNode`, `FlushOperationGroup`, `StatementShapeKey`) key the planner's
  graph, and a record's `hashCode`/`equals` is a bare `invokedynamic` that the
  x64 backend lowers to an unconditional deopt, so those bodies ran
  interpreter-only (1576 ns vs HotSpot 0.9 ns) — is FIXED via
  `InterpIntrinsic::{RecordHashCode,RecordEquals}` (3-5x). Root cause 2 is
  OPEN and is the blocker: 1362 of 1463 hot methods in this workload never
  JIT-compile at all (`tier_fail_count=3`, including 5-byte getters called
  500k+ times), leaving the planner ~2400x off HotSpot. Same family as
  tomcat doc 30. Removing the default today would fix 19 classes but hang 2.
- [`bulkid.*MutationStrategy*Test` — `testInsertSelect` last-row duplicate](bulkid-mutationstrategy-insertselect-lastrow-duplicate-20260727.md)
  (OPEN, genuine bug) — 11 classes, all failing on the same shared
  temp-table `row_number()`-based multi-table `INSERT ... SELECT` machinery
  (the `SqmMutationStrategy` under test only governs UPDATE/DELETE, not
  INSERT). Confirmed by HotSpot diff (CratonVM fails / HotSpot 6/6 clean),
  confirmed not a suite-subset/ordering artifact (fails alone, fresh
  process), confirmed not JIT-specific (`--nojit` fails identically). Root
  cause narrowed to a duplicate-iteration defect over the last row of a
  small in-memory result set, not fully pinned to a source line.
- [ordered-set aggregate as a window function — stuck on first partition's value](orderedsetaggregate-window-partition-value-staleness-20260727.md)
  (OPEN, genuine bug) — `CriteriaOrderedSetAggregateTest` (2/10 tests):
  `percentile_disc(...) over(partition by ...)` and
  `listagg(...) over(partition by ...)` return the *first* partition's
  computed value for every row instead of each row's own partition value.
  Confirmed by HotSpot diff (byte-identical SQL/params, HotSpot correct,
  CratonVM wrong). Possibly related substrate/family to the bulkid
  `row_number() over()` bug above (both are `OVER(...)` window-function
  row-buffering defects); not merged since concrete symptoms differ and
  neither is root-caused to a source line.
- [parameterized `OFFSET ? ROWS` silently not applied on grouped/ranked query](parameterized-offset-ignored-groupby-having-20260727.md)
  (OPEN, genuine bug) — `CriteriaOrderedSetAggregateTest` +
  `OrderedSetAggregateTest` (Criteria and HQL twins of the same
  `rank(...) ... group by ... having ... order by ... offset ?` query), both
  fail identically. HAVING filters correctly; the bound `OFFSET` parameter
  is simply not honored (first post-having row is not skipped). Confirmed by
  HotSpot diff (byte-identical SQL/params, HotSpot correct, CratonVM wrong).
- [two-predicate `AND` in `HAVING` returns zero rows](having-clause-and-conjunction-empty-result-20260727.md)
  (OPEN, genuine bug) — `CriteriaMultiselectGroupByAndOrderByTest` (2/6
  tests: `...AndHaving` and `...Subquery...AndHaving`) +
  `CriteriaFunctionParametersBindingTest::testPredicateArray` (1/2 tests).
  A `GROUP BY ... HAVING pred1 AND pred2` query returns an empty result set
  on CratonVM where HotSpot returns the correct single matching row —
  byte-identical SQL/bound-params both sides. Single-predicate `HAVING` and
  single-predicate `WHERE` parameter binding both work fine; only the
  two-predicate `AND`-in-`HAVING` shape is affected, across two unrelated
  fixtures/entity models.
- [dynamic-instantiation grouped join — one `VARCHAR` column reads the other row's value](dynamicinstantiation-groupby-join-varchar-column-row-swap-20260727.md)
  (OPEN, genuine bug) — `DynamicInstantiationWithJoinAndGroupAndOrderByByTest`
  (its only test). `id`/`sum` columns extract correctly for both rows of a
  2-row grouped join; only the `name` (VARCHAR) column on row 1 comes back
  as row 2's value. Confirmed by HotSpot diff (byte-identical SQL, HotSpot
  correct, CratonVM wrong).
- [`OVER(...)` window functions — partition/row-id correlation broken](windowfunction-partition-rowid-lookup-miss-shift-20260727.md)
  (OPEN, genuine bug) — 3 classes (`FormulaWithPartitionByTest`,
  `CriteriaWindowFunctionTest`, `WindowFunctionTest`). Not an H2-dialect
  capability gap (H2 2.4.240 genuinely supports window functions, confirmed
  clean on HotSpot). H2's internal per-partition row-id → value `HashMap`
  lookup loses correlation under CratonVM: a miss trips H2's own
  unreachable-by-design `"Feature not supported: Window function"` fallback,
  a bad hit produces an off-by-one/wraparound wrong value, and some queries
  return zero rows entirely. Same broad substrate family as the
  `bulkid`/`orderedsetaggregate` window-function docs below.
- [HQL `size(collection)` as a `GROUP BY` aggregate — every group reads the *last* group's value](size-groupby-aggregate-last-group-value-leak-20260727.md)
  (OPEN, genuine bug) — `ManyToManySizeTest` + `OneToManySizeTest` (11 of 16
  `@Test` methods). `size()` used in the `SELECT` list under `GROUP BY`
  collapses to the true count of the last-processed group for every row;
  `size()` used as a `WHERE`/restriction predicate is unaffected. Confirmed
  by HotSpot diff (byte-identical SQL, HotSpot correct, CratonVM wrong).

## Resolved (2026-07-27)

Both major clusters from the 2026-07-26 "passed"-category regression spike (248 FAIL, up from
single digits) cleared after merging `origin/dev` commit `13055f75c` ("fix(jit): String
compact-layout field offsets and the branch-join reload mirror") and rebuilding:

- `MappingXsdSupport.<clinit>` `StringIndexOutOfBoundsException` cluster (113 classes) — root-caused
  and FIXED (compact-ref-fields JIT regression in the class-init path).
  112/113 confirmed passing after the fix; the 1 straggler (`ScannerTest`) now fails on an
  unrelated pre-existing timeout, not this bug.
- `TableGroupJoinProducer.createTableGroupJoin` `NullPointerException` cluster (20 classes) — never
  formally root-caused (investigation was killed by an API rate limit before writing a doc), but
  confirmed resolved incidentally by the same merge: 20/20 now PASS. No doc was written since no
  root cause was ever captured.
