# Hibernate ORM suite — open known issues

## Open

- [`action.queue` GRAPH-default proof tests — LEGACY compatibility trade-off](actionqueue-graph-default-tests-legacy-tradeoff-20260727.md)
  (WON'T-FIX, expected) — `ActionQueueDefaultTest` and
  `InsertOrderingReferenceSeveralDifferentSubclassTest` fail because
  CratonVM intentionally defaults real-JDK `hibernate.flush.queue.type` to
  `legacy` (commit `0e87935f2`, fixing the `CycleBreaker` DFS hang). Confirmed
  by A/B repro: both pass clean with `-Dhibernate.flush.queue.type=graph`.

## Resolved (2026-07-27) — the `GROUP BY` / `OVER(...)` cluster, 7 docs, one root cause

Seven separate docs filed on 2026-07-27 all turned out to be **one** CratonVM defect:
the Rust override of `org.h2.expression.ExpressionColumn.getValue`
(`native-builtins/src/apps_h2.rs`) skipped H2's `SelectGroups` prologue, so in a
grouped or windowed query — where `Select.gatherGroup` has already scanned the source
to completion and `TableFilter.current` is pinned at the last row — every emitted row
read that last source row instead of its own buffered value.

Fixed by delegating to H2's own bytecode whenever `TableFilter.select.groupData` is
non-null (or the resolver is not a `TableFilter`); the ordinary non-grouped fast path
is untouched. Verified: all 21 affected classes, **82/123 → 123/123 tests passing,
identical to HotSpot**. Re-verified 2026-07-28 on dev `d0a6c7987`: 117/117 across the
20 lighter classes on both VMs, plus 6/6 for the heavy
`OracleInlineMutationStrategyIdTest` once the harness's 120 s-per-method cap is lifted,
plus 16/16 shapes matching HotSpot in a new Hibernate-free JDBC probe
(`docs/internal/repros/h2-groupdata-window-20260728/`), whose pre-fix control binary
reproduces every symptom shape on the same commit.

Retired to `docs/internal/fixed-suite-bugs/hibernate/`:

- `bulkid-mutationstrategy-insertselect-lastrow-duplicate-20260727-FIXED.md` (11 classes)
- `size-groupby-aggregate-last-group-value-leak-20260727-FIXED.md` (2 classes)
- `windowfunction-partition-rowid-lookup-miss-shift-20260727-FIXED.md` (3 classes)
- `orderedsetaggregate-window-partition-value-staleness-20260727-FIXED.md` (1 class)
- `having-clause-and-conjunction-empty-result-20260727-FIXED.md` (2 classes)
- `dynamicinstantiation-groupby-join-varchar-column-row-swap-20260727-FIXED.md` (1 class)
- `parameterized-offset-ignored-groupby-having-20260727-FIXED.md` (2 classes)

Consolidated root-cause write-up:
`h2-expressioncolumn-getvalue-native-bypasses-groupdata-20260727-FIXED.md`.

## Resolved (2026-07-27) — regression spike

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
