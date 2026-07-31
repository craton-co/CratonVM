# Hibernate ORM suite — open known issues

## Resolved (2026-07-30) — retired to `docs/internal/fixed-suite-bugs/hibernate/`

- **HIB-BYTEBUDDY ban removed for good.** The 302-class crash spike was an older runtime
  unmapping a compiled body while a live frame still executed it — an instruction-fetch fault
  mid-`ModifierReviewable$AbstractBase.matchesMask`, not a Byte Buddy miscompile. Byte Buddy is
  just the most JIT-churn-heavy code in the suite, so it is where that defect surfaced first, and
  the 2026-07-29 re-instatement hid it rather than fixing it. With the three JIT code-lifetime
  fixes on `dev` and no blanket guard, the exact 302-class manifest passes in **both** modes:
  301 PASS / 1 assumption-abort / 0 FAIL / 0 CRASH / 0 HANG, identical counts in each. A
  `skip_list` unit test now fails the build if a blanket `net/bytebuddy/` guard is ever re-added.
  Full write-up: `docs/internal/fixed-suite-bugs/hibernate/hib-bytebuddy-20260730-FIXED.md`.

## Open

- [Native ANTLR intrinsics lose object roots under the moving young collector](antlr-native-roots-moving-young-hql-misparse-20260730.md)
  (OPEN; root cause identified, many instances fixed across two independent efforts) —
  `ASTParserLoadingTest` rejects valid HQL nondeterministically. `antlr_intrinsics.rs` holds raw
  `ObjectRef` locals and whole `Vec<ObjectRef>` config snapshots across allocating calls, so a
  moving young collection links dead addresses into the parser graph; the poisoned config is then
  memoized as a DFA edge, which is why one mis-timed collection breaks a whole grammar path for
  the rest of the process. **Not** the trivial-accessor fast path that was blamed and deleted on
  2026-07-29: the verifier reports zero field-resolution divergences, and
  `CRATONVM_NO_MOVING_YOUNG=1` passes 106/106. The 302-class corpus is clean on the current tree,
  but the unsafe idiom is still the file's default style — the doc recommends a scoped handle type
  to make it unrepresentable rather than further per-site auditing.

- [`action.queue` GRAPH-default tests — blocked by flush-planner throughput](../../internal/fixed-suite-bugs/hibernate/actionqueue-graph-default-tests-legacy-tradeoff-20260727-FIXED.md)
  (OPEN; one of two root causes fixed) — real-JDK CratonVM defaults
  `hibernate.flush.queue.type` to `legacy`, which gates **19 of the 25
  `action.queue` classes**: 2 fail outright and 17 self-abort via
  `Assumptions.abort("Skipping GRAPH test with non-GRAPH queue type")` (the
  earlier WON'T-FIX doc reported only the 2). Root cause 1 — records
  (`GroupNode`, `FlushOperationGroup`, `StatementShapeKey`) key the planner's
  graph, and a record's `hashCode`/`equals` is a bare `invokedynamic` that the
  x64 backend lowers to an unconditional deopt, so those bodies ran
  interpreter-only (1576 ns vs HotSpot 0.9 ns) — is FIXED via
  `InterpIntrinsic::{RecordHashCode,RecordEquals}` (3-5x on record-keyed
  collections). Root cause 2 is OPEN and is the blocker: 1362 of 1463 hot
  methods in this workload never JIT-compile at all (`tier_fail_count=3`,
  including 5-byte getters called 500k+ times), leaving the planner ~2400x off
  HotSpot. Same family as tomcat doc 30. Removing the default today would
  un-gate 19 classes but hang 2 (`joinedsubclassbatch`, >900 s each).

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
