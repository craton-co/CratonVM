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

## HANG classes in the fresh 4548-class run (2026-07-31) — one harness gap (now fixed, and hiding two real VM defects), two stale-binary/contention margins

Four more classes report `HANG` (`process-died rc=124`) in the same
2026-07-30/31 categorize run:

- **`boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest` — harness
  gap RESOLVED 2026-07-31; the class turned out to be hiding two real VM
  defects.** The lost runner accommodation is re-implemented and now durable
  (tracked `apps/hib-suite-runner/class-overrides.tsv` + `run-hib.sh`,
  force-added past the blanket `apps/` ignore, LF-pinned, self-reporting
  `overrides=N (loaded)`) — retired to
  `../../internal/fixed-suite-bugs/hibernate/qualfiedtablenaming-runner-timeout-floor-lost-20260731-FIXED.md`.
  **The class now PASSES on the JIT lane** — `132/132 failed=0`, 3 runs for 3,
  identical to the HotSpot control — as of 2026-08-01. Getting there took three
  VM fixes, because running it to completion for the first time disproved the
  inherited "clean but slow, just needs a bigger timeout" premise: HotSpot does
  it in 119.7 s on the same `-Xmx1500m`, while CratonVM failed in both modes,
  for reasons now tracked on their own:
  - JIT on (the suite's lane) — `OutOfMemoryError` on a 49 %-full heap at
    ~41 min. **FIXED 2026-07-31**, retired to
    [`../../internal/fixed-suite-bugs/hibernate/gc-overhead-limit-spurious-oom-at-half-full-heap-20260731-FIXED.md`](../../internal/fixed-suite-bugs/hibernate/gc-overhead-limit-spurious-oom-at-half-full-heap-20260731-FIXED.md).
    It was **not** a manifestation of the moving-young gap, as first reported: it
    was an independent regression in which the non-moving sweep's selective
    promotion — the young generation's only drain — had been switched off by a
    flag that changed meaning underneath its gate. With that restored the class
    runs to completion with **zero** forced GCs, where it previously took thirty
    and died on the eighth unproductive one.
    Reaching the end of the class for the first time then exposed a **second**
    defect, in `[class-template-invocation:#12]`, which had been unreachable
    behind the OOM: selective promotion was evacuating roots published as
    "movable" precise-JIT roots on cycles whose coverage proof had *failed*.
    Also **FIXED**, retired to
    [`../../internal/fixed-suite-bugs/hibernate/invocation12-late-phase-instability-movable-jit-root-20260801-FIXED.md`](../../internal/fixed-suite-bugs/hibernate/invocation12-late-phase-instability-movable-jit-root-20260801-FIXED.md).
    With both fixes the class runs **`132/132 failed=0`, 3 runs for 3** —
    identical to the HotSpot control.

    One residual, unchanged and not a regression: ~35–50 min against HotSpot's
    120 s. That part *is* the
    [moving-young-inert-under-JIT](moving-young-inert-under-jit-throughput-tax-20260730.md)
    throughput tax, and it stays there.
  - `--nojit` — [SIGSEGV from stale chain cursors in `map_resize_inner`](map-resize-unpinned-chain-cursors-nojit-segv-20260731.md)
    at ~20 min. Note this **inverts** the 2026-07-22 advice to force `--nojit`
    for this class: `--nojit` is now the worse mode. A second corrupt writer in
    the same runs (`HIB-WEAKREF-RECYCLE.1`, post-GC weak/phantom referent
    restore) was root-caused and **fixed** on the same branch.

  The class's `MutableBigInteger` AIOOBE quarantine (`41cdfdf94`) is untouched
  and not in question.

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

- **Native ANTLR intrinsics lose object roots under the moving young collector** — FIXED
  2026-07-31. `ASTParserLoadingTest` rejected valid HQL nondeterministically because
  `antlr_intrinsics.rs` held raw `ObjectRef` locals — and whole `Vec<ObjectRef>` config
  snapshots — across allocating calls, so a moving young collection linked dead addresses into
  the parser graph and the poisoned config was memoized as a DFA edge. **Not** the
  trivial-accessor fast path that was blamed and deleted on 2026-07-29. Three per-site passes
  each found "a few more" and none had a completion signal; the fourth changed the
  representation: all 344 `pin_native_root` / `read_native_pin` / `unpin_native_roots` calls are
  gone, replaced by `NativeHandleScope` / `NativeHandle`; config sets are walked by index out of
  the rooted set; a guard test keeps the raw API out. Eleven further groups of live unrooted sites
  (~29 functions) were fixed on the way through. Full write-up:
  `docs/internal/fixed-suite-bugs/hibernate/antlr-native-roots-moving-young-hql-misparse-20260730-FIXED.md`.

## Open

- [HQL ordinal parameter silently dropped — `ordinal parameters []` under JIT](hql-ordinal-parameter-dropped-under-jit-20260731.md)
  (OPEN; observed once, cause not located) — `ASTParserLoadingTest#testComponentNullnessChecks`
  failed 1 run in 14 under JIT with `No parameter labelled '?1' in query with ordinal parameters []`
  and has not recurred (a follow-up 8-run interleaved A/B was clean on both binaries).
  The query parses without a syntax error but its ordinal parameter never reaches
  `ParameterMetadataImpl`, so this is a *missing production*, not the rejected-parse shape of the
  (now fixed) ANTLR moving-young root defect — zero `SyntaxException`s appeared in any of the
  twelve witness runs. `HqlParseStress` now asserts that every parameter marker survives into
  `statement().getText()`; 3500 parses per arm across jit/nojit x default/GC-stress reproduce
  nothing on either binary, so the defect is likely downstream of the parse tree.

- [`map_resize_inner` publishes stale chain heads into the resized bucket array](map-resize-unpinned-chain-cursors-nojit-segv-20260731.md)
  (OPEN; root cause located, fix not landed) — `HIB-MAPRESIZE-STALE.1`. The JDK-style
  split walk holds `old_b`/`new_buckets`/`node_val` and all four lo/hi head/tail cursors
  as bare Rust locals across two REFERENCE-typed `set_field` stores, each of which can
  allocate a remembered-set entry through the write barrier and therefore complete a
  moving young GC. The stale `lo_head`/`hi_head` are then published into `new_buckets`,
  so the map permanently holds dangling chain heads and every later put walks freed
  memory. SIGSEGV at ~17-20 min, three for three. Same class of bug — and the same fix
  shape — as the ANTLR root defect retired above (which converted its whole module to
  `NativeHandleScope` rather than patching sites one at a time) and as
  `native_map_put_evict_pinned`'s own existing pin discipline a few hundred lines away in
  the same file.

- ~~Spurious `OutOfMemoryError` with 570 MB free~~ — `HIB-GCOVERHEAD-HALFFULL.1`,
  **FIXED 2026-07-31**, retired to
  [`../../internal/fixed-suite-bugs/hibernate/gc-overhead-limit-spurious-oom-at-half-full-heap-20260731-FIXED.md`](../../internal/fixed-suite-bugs/hibernate/gc-overhead-limit-spurious-oom-at-half-full-heap-20260731-FIXED.md).
  The report's `promoted=0`-on-every-cycle observation was the whole story, and it
  was **not** downstream of the moving-young fallback as that report concluded: the
  non-moving sweep's selective promotion, the young generation's only young→old
  drain under a live JIT frame, was gated on
  `moving_young_coverage_incomplete()` — a flag that had exactly one (cross-thread
  takeover) caller when the gate was written and, after `arch-2026-07-26` reused it
  for relocation policy, became true on essentially every JIT-active collection.
  Young filled with live objects it could not evict; the streak latched; the VM
  OOM'd at 49 % full. Fixed by splitting the two verdicts
  (`gc_quiescence::unrewritable_peer_state`), plus the missing free-space half of
  HotSpot's `UseGCOverheadLimit` as a safety net so the next drain defect is slow
  rather than fatal.

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
