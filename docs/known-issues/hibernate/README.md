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
  **The class still does not pass.** Running it to completion for the first time
  disproved the inherited "clean but slow, just needs a bigger timeout" premise:
  HotSpot does it in 119.7 s on the same `-Xmx1500m`, while CratonVM fails in
  both modes, for two newly-found reasons now tracked on their own:
  - JIT on (the suite's lane) — [`OutOfMemoryError` on a 49 %-full heap](gc-overhead-limit-spurious-oom-at-half-full-heap-20260731.md)
    at ~41 min. A manifestation of the already-open
    [moving-young-inert-under-JIT](moving-young-inert-under-jit-throughput-tax-20260730.md)
    gap, as a *correctness* failure rather than only a throughput tax.
  - `--nojit` — [the collector leaves reference fields UN-FORWARDED](map-resize-unpinned-chain-cursors-nojit-segv-20260731.md).
    The SIGSEGV is **fixed** (the class now completes, `rc=0` @ 5110 s, where it
    crashed at 2103–2611 s), but it still does not match HotSpot: `found=99` vs
    `132`, with JUnit `TestPlan` losing identifiers. That doc's original
    `map_resize_inner` attribution is RETRACTED. A second corrupt writer in
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

- **A user lambda could be routed to the collector natives under the JIT** — FIXED
  2026-07-31, closing the HQL ordinal-parameter report. `Collector.accumulator()` and its
  siblings return synthetic objects whose class name IS the SAM interface, so the natives
  serving them are registered on `java/util/function/{Supplier.get, BiConsumer.accept,
  Function.apply, BinaryOperator.apply}`. A lambda proxy has no class of its own in the class
  store, so the JIT's by-name dispatch bails resolved it as its functional interface and ran
  those natives instead of the lambda: `Supplier.get` returned an empty `ArrayList`,
  `Function.apply` returned its argument, `BiConsumer.accept(a,b)` called `a.add(b)`. Three of
  the four are silent; the fourth is the `NoSuchMethodError: ....add(Ljava/lang/Object;)Z` that
  failed three `ASTParserLoadingTest` tests on **every** JIT run. Needs the site compiled
  (the interpreter resolves proxies through the registry first) and megamorphic. A shared
  `try_lambda_proxy_sam_dispatch` now guards both bails.
  `FunctionalInterfaceHijackProbe.java` is the reduced witness. The report that prompted the
  hunt — a one-in-fourteen `ordinal parameters []` — never reproduced and is **not** attributed
  to this; read the write-up's "What is and is not proven" before citing it.
  Full write-up: `docs/internal/fixed-suite-bugs/hibernate/hql-ordinal-parameter-dropped-under-jit-20260731-FIXED.md`.

## Open

- [`Type.getTypeName()` dispatches on `java/lang/Integer` during a SessionFactory rebuild cascade](gettypename-wrong-receiver-in-sessionfactory-rebuild-cascade-20260801.md)
  (OPEN; correlations established, **not reproduced**) — a `Class` mirror resolving as the class it
  DESCRIBES, in `JavaTypeRegistry.addBaselineDescriptor`'s inlined `getTypeName()` call. Exact
  correlation with a SessionFactory rebuild cascade: the 2 runs of 24 that showed it did 21
  bootstraps and 32 warnings each and were killed at the 1200s cap; every run that did 3-4
  bootstraps showed none. During the cascade the log has no test activity at all. The VM recovers
  and no test fails. Ruled out by measurement, not argument: the `getJavaType()` accessor (810k
  checks), `Type.getTypeName()` dispatch over 30 mirrors (1.8M checks), and `VIRTUAL_TARGET_CACHE`
  pointer recycling (the cached name is a pure function of the ClassId half of the key). The
  blocker is inducing the cascade — every attempt topped out at 5 bootstraps.

- [Old-generation header corruption kills `DefaultCatalogAndSchemaTest` under `--nojit`](map-resize-unpinned-chain-cursors-nojit-segv-20260731.md)
  (NARROWED, still OPEN) — `HIB-MAPRESIZE-STALE.1`. **The `map_resize_inner` attribution
  this entry used to carry is retracted**: a reference-typed store is not a GC point at
  all (`gen_heap::write_barrier` never allocates from the Java heap), which is why the
  pin refactor that premise called for landed and changed nothing. Three defects were
  tangled here. Two are fixed — a whole class of collection natives holding heap refs
  across their own allocations and Java callbacks (`fix/hib-mapresize-put-stale-20260731`,
  ~40 sites behind a new `rooted_across` helper, caught deterministically in ~90 s by the
  new `RMapGcStress`), and the corrupt-header diagnostic that `Debug`-formatted an invalid
  `ObjectKind` and walked a wild pointer in `core::fmt` (`22107d512`). The residual is
  what makes an old-gen header garbage in the first place: arm C logs 48 rejected headers
  with `kind=0x3a` (ASCII `':'` — text written over a header) and then SIGSEGVs on a heap
  address. Crash time has moved 2103 s → 2312 s → 2611 s across the three arms. The one
  lead is a `CRATONVM_DBG_STALE_OBJREF` hit in a native reached from
  `ClassLoaderServiceImpl.classForName` — the same bug shape, in the class-loading
  natives.

- [Spurious `OutOfMemoryError` with 570 MB free](gc-overhead-limit-spurious-oom-at-half-full-heap-20260731.md)
  (OPEN; proximate cause confirmed by differential) — `HIB-GCOVERHEAD-HALFFULL.1`. The
  GC-overhead limit latches after 8 cycles that each free < 2 % of *capacity*, on a heap
  that is only 49 % full with `promoted=0` on every cycle — the case
  `note_gc_productivity`'s "wedged full old gen" reasoning explicitly does not cover.
  `CRATONVM_GC_OVERHEAD_LIMIT=0` runs 85 min without an OOM where the default dies at 41.
  Underneath it is the moving-young fallback below, showing up as a correctness failure
  rather than only a throughput tax.

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
