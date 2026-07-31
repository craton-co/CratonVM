# Hibernate ORM suite — open known issues

## Audited, not a bug (2026-07-31) — ABORTED classes in the fresh 4548-class run

Three classes show class-level **ABORTED** in the fresh 2026-07-30/31 full-suite
categorize run (`apps/hib-suite-runner/analysis/06-full-suite-categorize-20260730/all-4548-classes-status.tsv`)
but are confirmed benign, not regressions and not CratonVM bugs — isolated
`CratonRunner` re-runs against both the fresh CratonVM binary and plain
HotSpot (same JDK, same classpath, no CratonVM in the loop) produce
byte-identical found/started/ok/failed/aborted counts:

- `org.hibernate.orm.test.bytecode.enhancement.basic.InheritedTest` and
  `.MappedSuperclassTest` — `found=4 ok=3 aborted=1` on both VMs. The abort is
  `extendedEnhancementTest()`'s own `assumeTrue(...isAssignableFrom...)`,
  which is false-by-construction under each class's eager
  (non-lazy-loading) `@CustomEnhancementContext`. See the RE-VERIFICATION
  2026-07-31 section of
  `../../internal/fixed-suite-bugs/hibernate/hib-bytecode-enhancement-loader-faithful-linking-FIXED.md`.
- `org.hibernate.orm.test.manytomanyassociationclass.surrogateid.generated.ManyToManyAssociationClassGeneratedIdTest`
  — `found=6 ok=3 aborted=3` on both VMs. The 3 overridden test methods each
  call `assumeFalse(queueType == QueueType.GRAPH, ...)`, which self-skips
  under Hibernate's (now-restored) upstream GRAPH default. See §8 of
  `../../internal/fixed-suite-bugs/hibernate/hib-bytebuddy-20260730-FIXED.md`
  and `../../internal/fixed-suite-bugs/hibernate/actionqueue-graph-default-tests-legacy-tradeoff-20260727-FIXED.md`.

No doc was moved or newly filed for any of these three — do not re-open them
as regressions on a future ABORTED sighting without first checking whether
HotSpot aborts the same tests for the same reason.

## HANG classes in the fresh 4548-class run (2026-07-31) — one real harness gap, two stale-binary/contention margins

Four more classes report `HANG` (`process-died rc=124`) in the same
2026-07-30/31 categorize run:

- **[`boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest` — the runner's per-class timeout accommodation is gone](qualfiedtablenaming-runner-timeout-floor-lost-20260731.md).**
  A real, currently-open harness gap, **not** a VM regression: the
  "Resolved 2026-07-22" fix recorded in
  `../../internal/fixed-suite-bugs/hibernate/qualfiedtablenaming-hang-cluster-20260721-FIXED.md`
  (a 3600s per-class timeout floor + forced `--nojit` in `run-hib.sh`) is not
  present in the current `run-hib.sh` — that script is wholly gitignored
  (`apps/`), carries no commit history, and has already lost driver-file state
  once before (2026-07-16 truncation). The class's own correctness fix
  (`MutableBigInteger` AIOOBE quarantine, `41cdfdf94`) is untouched and not in
  question; solo repro this session reconfirms genuine, continuous CPU-bound
  work with zero stall signature, matching the class's own well-established
  "clean but slow" profile. See the doc for the recommended re-implementation.

- **`bulkid.OracleInlineMutationStrategyIdTest` — stale binary, not a
  regression; already faster on current `dev`.** This class is a long-known,
  already-documented timeout-marginal residual (see the `GROUP BY` cluster
  entry below and
  `../../internal/fixed-suite-bugs/hibernate/h2-expressioncolumn-getvalue-native-bypasses-groupdata-20260727-FIXED.md`'s
  residual section). The categorize run's binary
  (`CratonVM-hib-local-0712-v3` @ `8e8a7b8cd`) predates two fixes merged into
  `dev` hours later the same day/night
  (`f78b72670` relocation-safety gate scoping, `11901e9a6` moving-young
  liveness veto — both folded into `dev` via `edd95bc89`) that took this exact
  class from ~322-442s down to 118.9s on a quiet host — see
  `../../internal/repros/hib-five-20260730/RESULTS-final-20260731.md`. Solo
  repro this session on the *same pre-fix binary* the categorize run used
  completed in 212.7s (`found=6 ok=6 failed=0`) — under the 300s cap on its
  own, so the full run's 8-way shard contention is what tipped this
  marginal-timing class into a `HANG`, not a code defect. No doc needs
  correcting; this is the already-understood "timeout-marginal class +
  contended host" pattern, now additionally resolved on `dev` tip.

- **`batch.BatchTest` and `batchfetch.DynamicBatchFetchTest` — same
  "timeout-marginal class + contended host" pattern; moving-young mechanism
  explicitly ruled out.** Both are long-tracked members of
  `../../internal/fixed-suite-bugs/hibernate/hib-120s-junit-timeout-cluster-20260716.md`'s
  generic interpreter/JDBC-throughput cluster (previously confirmed passing
  clean at 98-126s on a quiet host). Solo repro this session across three
  configurations -- the categorize run's own binary, that same binary with
  `CRATONVM_NO_MOVING_YOUNG=1`, and a fresh binary containing the 2026-07-30
  moving-young-liveness fix (`11901e9a6`) that measurably speeds up
  `OracleInlineMutationStrategyIdTest` above -- all three land within seconds
  of each other (213-255s) and trip the identical Hibernate-internal 120s
  per-method `TimeoutException`; `CRATONVM_GC_STATS=1` printed **zero** `[GC]`
  lines in any of the three, i.e. neither class ever triggers a young
  collection, so a fix that only pays off when a moving collection is
  requested cannot help either one. Unlike `OracleInlineMutationStrategyIdTest`,
  these two are **not** sped up by the fresh binary. Conclusion: unchanged
  "generic throughput margin, contended-host-dependent" verdict, not the
  moving-young tax, not reopened. See the hib-120s doc's own 2026-07-30/31
  recurrence section for the full A/B table.

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
