# Hibernate suite — 61-class scattered HANG longtail (likely mixed genuine-slowness + host-load artifacts)

| | |
|---|---|
| **Status** | 🟡 UNCLASSIFIED — not individually root-caused; likely a mix of real slowness and this run's heavy host contention. HotSpot confirmation pending. |
| **Discovered** | 2026-07-11 full 4548-class suite audit, `dev` post `44f16ee2`+, `TIMEOUT=300`. |

61 classes hang (`rc=124`) that don't fit the two dedicated hang clusters in
this audit
([hib-cascade-multipathcircle-hang-cluster.md](hib-cascade-multipathcircle-hang-cluster.md),
[hib-immutable-entitywithmutablecollection-hang-cluster.md](hib-immutable-entitywithmutablecollection-hang-cluster.md)).
Unlike those two clusters (100% consistent hang rate across a tight,
combinatorially-related test family — strong evidence of a genuine shared
bug), this list is a scattered grab-bag across ~20 unrelated packages with
no obvious common thread.

## Known precedent: this session has repeatedly observed this list is NOT stable

Several of these exact classes have been individually tracked earlier in
this investigation and shown **inconsistent** pass/fail/hang behavior across
separate reruns of the identical binary and command on this same (heavily
shared, multi-tenant) host:

- `query.hql.FunctionTests`, `jpa.criteria.InPredicateTest`,
  `sql.exec.SmokeTests`, `sql.storedproc.{ResultMappingTest,StoredProcedureTest}`,
  `hql.ASTParserLoadingTest`, `boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest`,
  `type.temporal.{OffsetTimeTest,OffsetDateTimeTest,ZonedDateTimeTest}` —
  every one of these has been seen this session in at least two different
  states (PASS, FAIL, HANG, or CRASH) across different reruns, sometimes
  within minutes of each other, correlated with how heavily loaded this
  shared Azure host was at the time. `boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest`
  specifically was confirmed non-deterministic under controlled A/B testing
  (see the RESOLVED doc for the JIT `getfield` regression that explained
  its earlier flakiness — [hib-global-temptable-nondeterministic-sigsegv-20260710-RESOLVED.md](../../internal/fixed-suite-bugs/hib-global-temptable-nondeterministic-sigsegv-20260710-RESOLVED.md)).
- This matches the broader "generic 120s/300s timeout wall" pattern
  documented in
  [hib-linux-fail-bucket-triage-20260703.md](../hib-linux-fail-bucket-triage-20260703.md),
  where several classes were confirmed to pass cleanly on an uncontended
  rerun after appearing to hang/timeout under load.

Given this precedent, treat this list as **not confirmed CratonVM bugs**
until reproduced on a quiet host or with a substantially longer timeout.

## Full list (61)

```
annotations.onetomany.OneToManyTest
annotations.xml.ejb3.Ejb3XmlElementCollectionTest
annotations.xml.ejb3.Ejb3XmlOneToOneTest
batch.BatchAndClassIdCollectionTest
batch.BatchAndUserTypeIdCollectionTest
batch.CompositeIdAndElementCollectionBatchingTest
batch.CompositeIdWithOneFieldAndElementCollectionBatchingTest
batch.SetAndBagCollectionTest
batchfetch.BatchFetchRefreshTest
batchfetch.BatchFetchTest
batchfetch.DynamicBatchFetchTest
batchfetch.NestedLazyManyToOneTest
boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest
boot.jaxb.mapping.HbmTransformationJaxbTests
bootstrap.scanning.JarVisitorTest
bulkid.OracleInlineMutationStrategyIdTest
bytecode.enhancement.lazy.proxy.FetchGraphTest
bytecode.enhancement.lazy.proxy.inlinedirtychecking.LoadAndUpdateEntitiesWithCollectionsTest
bytecode.enhancement.lazy.proxy.inlinedirtychecking.dynamicupdate.DynamicUpdateAndCollectionsTest
cache.CollectionCacheEmbeddedIdKeyTest
connections.ThreadLocalCurrentSessionTest
entitygraph.EntityGraphBatchSizeTest
hql.ASTParserLoadingTest
hql.BulkManipulationTest
id.enhanced.OptimizerConcurrencyUnitTest
joinedsubclassbatch.IdentityJoinedSubclassBatchingTest
joinedsubclassbatch.JoinedSubclassBatchingTest
jpa.callbacks.PreUpdateNewUnidirectionalBagTest
jpa.callbacks.ProtectedConstructorTest
jpa.callbacks.xml.EntityListenerViaXmlTest
jpa.criteria.InPredicateTest
jpa.criteria.fetchscroll.CriteriaScrollFetchTest
jpa.criteria.paths.DynamicModelSingularAttributeJoinTest
jpa.criteria.query.CriteriaDefinitionTest
jpa.emops.cascade.CascadePersistTest
jpa.graphs.LoadEntityGraphWithCompositeKeyCollectionsTest
jpa.graphs.queryhint.EmbeddableQueryHintEntityGraphTest
jpa.metamodel.JpaMetamodelDisabledPopulationTest
jpa.naturalid.ImmutableNaturalIdTest
jpa.orphan.onetomany.DeleteSharedOneToManyOrphansTest
jpa.procedure.StoredProcedureResultSetMappingTest
jpa.ql.JoinTableOptimizationTest
jpa.ql.TreatKeywordTest
jpa.query.CachedQueryShallowMultiselectTest
jpa.query.QueryTest
jpa.query.TupleNativeQueryTest
manytomany.batchload.BatchedManyToManyTest
ops.MergeMultipleEntityCopiesAllowedLoggedTest
ops.MergeMultipleEntityCopiesAllowedTest
ops.MergeTest
query.hql.FunctionTests
readonly.ReadOnlyTest
readonly.ReadOnlyVersionedNodesTest
softdelete.collections.FetchLoadableTests
sql.exec.SmokeTests
sql.storedproc.ResultMappingTest
sql.storedproc.StoredProcedureTest
type.temporal.OffsetDateTimeTest
type.temporal.OffsetTimeTest
type.temporal.ZonedDateTimeTest
version.db.DbVersionTest
```

## Next steps (not yet done)

- Rerun this exact 61-class list, ideally on a quiet/dedicated host (or at
  minimum, coordinate with concurrent host activity), with `SHARDS=1` (no
  intra-run contention) and a longer `TIMEOUT` (e.g. 1500s) to separate
  genuinely-hanging classes from ones that just need more wall-clock time
  under load.
- Any class that reproduces a hang cleanly on a quiet host with a generous
  timeout should get its own dedicated doc with a `gdb`-backtrace-based
  root cause, following the pattern established for the two confirmed hang
  clusters in this audit.
- Confirm CratonVM-specificity with a HotSpot run (pending) — several of
  these package prefixes (`batch.*`, `batchfetch.*`, `jpa.query.*`) are
  large multi-method test classes that could legitimately be slow on any
  JVM under this harness's TRACE-level SQL logging configuration; don't
  assume CratonVM-specific without confirmation.

## Resolution (2026-07-15)

Resolved and retired. A serial Azure rerun covered every class in this list.
The two reproducible residuals were `OptimizerConcurrencyUnitTest` and
`SmokeTests.testQueryConcurrency`, both caused by the executor compatibility
layer returning placeholder `FutureTask` objects and intercepting
`AbstractExecutorService.invokeAll`. Real JDK methods consequently observed
incomplete or incorrectly represented futures and collections.

The executor bridge now returns initialized, completed `CompletableFuture`
objects for native submit paths and leaves `invokeAll` to the JDK's own
`AbstractExecutorService` implementation. This also cleared the temporal
residuals in `ZonedDateTimeTest`.

Final Azure validation using the release binary built from this change:

- `SmokeTests`: 17/17 passed in 119997 ms.
- `OptimizerConcurrencyUnitTest`: 12/12 passed in 144260 ms.
- `ZonedDateTimeTest`: 196 found, 132 passed, 0 failed (64 suite aborts).
- The original serial rerun completed the remaining 58 documented classes
  without failures; pre-existing JUnit skips/aborts are retained as suite
  outcomes, not CratonVM errors.
