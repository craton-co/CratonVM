# Hibernate suite — remaining ~89-class scattered failure longtail (triage catalog, not individually root-caused)

| | |
|---|---|
| **Status** | 🟡 PARTIALLY RESOLVED — the current Azure checkpoint closes the UUID timeout mechanism and validates the full `sql.exec.*` and `InstantiationException` clusters. The remaining catalog still needs its own current-binary rerun. |
| **Discovered** | 2026-07-11 full 4548-class suite audit, `dev` post `44f16ee2`+. |

After pulling out every cluster with a clear shared signature elsewhere in
this audit, the original catalog contained 110 classes with no confirmed
shared root cause. A complete current-`dev` sweep subsequently passed all 94
members of the resolved
[statistics-counter cluster](../../internal/fixed-suite-bugs/hibernate-statistics-longadder-counters-zero-FIXED.md)
and all 21 statistics-adjacent candidates from this catalog. Those candidates
are no longer open entries, leaving **about 89 classes** here. Given the scale,
this doc cataloged the remaining classes with visible sub-patterns rather than
individually root-causing each one. The 2026-07-14 checkpoint below supersedes
the affected historical cluster entries; do not use the original approximate
class count as a current failure count.

## 2026-07-14 Azure checkpoint — current status

The dedicated Azure worktree was built as
`cvhib-longtail-overlayfix-20260714-azure-012`. Three runtime fixes were
committed and pushed on the dedicated branch before this checkpoint:

- H2 native-path and JIT argument-handling corrections;
- GC-safe, non-interned transient UUID strings; and
- direct removal of a reclaimed collection-overlay key from its known owner.

The last change removes the quadratic GC cleanup that the debugger captured in
`native_collections::remove_overlay_owner_key` while AssertJ compared UUIDs.
The pathological 300,000-comparison reproducer completed in 12.7 seconds
(rather than stalling around 280,000 comparisons). Both real
`UUidV6V7GeneratorTest` methods passed under `--nojit`: v6 in 71.967s and v7
in 72.406s.

The following historical catalog entries now have a complete current-binary
pass and are not open residuals:

- all 11 `sql.exec.*` classes listed below: 61 tests passed;
- all six `InstantiationException` candidates listed below: 12 tests passed;
- `annotations.fetchprofile.FetchProfileTest`: 7 tests passed in 28.261s.

`sql.exec.EmbeddedIdEntityTest` specifically passed all six methods. Its
historical Hibernate Models annotation-supplier diagnosis no longer reproduces
on this runtime path, so it must not be used as the current root-cause theory.

The timeout batch was intentionally stopped during `batch.BatchTest` at the
user-requested checkpoint. The remaining timeout, schema-generation, and
one-off entries have not been reclassified by this checkpoint; this note stays
under `docs/known-issues` until those residuals are handled.

## Visible sub-patterns worth investigating first

**`InstantiationException: Could not instantiate entity` (6 classes, resolved
in the 2026-07-14 Azure checkpoint)** — originally suspected to be the
constructor-invocation sibling of the
[PropertyAccessException setter cluster](hib-bytecode-enhancement-propertyaccessexception-setter-cluster.md):
```
annotations.cid.EmbeddedIdLazyOneToOneCriteriaQueryTest
bytecode.enhancement.lazy.proxy.EagerOneToOneMappedByInDoubleEmbeddedTest
bytecode.enhancement.lazy.proxy.LazyOneToOneMappedByInDoubleEmbeddedTest
bytecode.enhancement.lazy.proxy.LazyOneToOneMappedByInEmbeddedTest
bytecode.enhancement.merge.CompositeMergeTest
bytecode.enhancement.refresh.RefreshTest
```

**`sql.exec.*` bare `AssertionError` (11 classes, resolved in the 2026-07-14
Azure checkpoint)** — historical tight package cluster:
```
sql.exec.EmbeddedIdEntityTest
sql.exec.EntityWithEmbeddedIdTest
sql.exec.EntityWithNotAggregateIdTest
sql.exec.manytoone.EntityWithLazyManyToOneSelfReferenceTest
sql.exec.manytoone.EntityWithManyToOneJoinTableTest
sql.exec.manytoone.EntityWithManyToOneSelfReferenceTest
sql.exec.manytoone.ManyToOneTest
sql.exec.onetoone.EntityWithLazyBidirectionalOneToOneTest
sql.exec.onetoone.EntityWithOneToOneJoinTableTest
sql.exec.onetoone.EntityWithOneToOneSharingPrimaryKeyTest
sql.exec.onetoone.EntityWithOneToOneTest
```

**`TimeoutException: ... timed out after 120 seconds` (13 classes)** —
matches the previously-established "generic timeout wall" pattern (see
[hib-linux-fail-bucket-triage-20260703.md](../hib-linux-fail-bucket-triage-20260703.md)
and [hib-generic-timeout-hang-longtail.md](hib-generic-timeout-hang-longtail.md)) —
these are FAIL (not HANG) because they hit JUnit's *internal* 120s
per-method timeout while the harness's own 300s process timeout hadn't yet
elapsed, so the process survives and reports a clean JUnit failure instead
of getting killed. Same "needs a quiet host + longer timeout to confirm
genuine vs. load-driven" caveat applies:
```
annotations.fetchprofile.FetchProfileTest
batch.BatchTest
id.uuid.rfc9562.UUidV6V7GeneratorTest
insertordering.InsertOrderingRCATest
jpa.CompositeIdRowValueTest
jpa.cascade.DeleteOrphanTest
jpa.compliance.ModulusTest
jpa.criteria.TreatDisjunctionTest
jpa.criteria.valuehandlingmode.inline.EqualityComparisonTest
jpa.criteria.valuehandlingmode.inline.NonPkAssociationEqualityPredicateTest
jpa.emops.MergeNullCollectionTest
jpa.exception.ExceptionTest
jpa.integrationprovider.IntegrationProviderSettingByClassUsingPropertiesTest
```

**`jpa.schemagen.*` "expected: not `<null>`" (3-4 classes)** — schema-
generation output is null where a real value was expected:
```
jpa.schemagen.JpaFileSchemaGeneratorTest
jpa.schemagen.JpaSchemaGeneratorTest
jpa.schemagen.iso8859.JpaSchemaGeneratorWithoutHbm2DdlCharsetNameTest
jpa.schemagen.iso8859.SchemaCreateDropWithHbm2DdlCharsetNameTest  (expected:<true> but was:<false> — related but not identical)
```

## Everything else (no visible pattern, one-off signatures)

```
annotations.entity.BasicHibernateAnnotationsTest           :: AssertionFailedError (bare)
bootstrap.binding.annotations.embedded.EmbeddedCheckQueryExecutedTest :: AssertionError (bare)
bootstrap.binding.annotations.embedded.EmbeddedCircularFetchTests     :: AssertionError (bare)
bootstrap.scanning.PackagedEntityManagerTest                :: IllegalArgumentException: excludehbmpar/META-INF/orm2.xml doesn't exist or can't be accessed  (same family as the already-tracked jar-scanning bug — see hib-proxyclassreuse-loader-blind-class-resolution.md)
bytecode.enhancement.lazy.LazyOneToOneRemoveFlushAccessTest :: AssertionError: Test should work with transient strictness disabled, instead threw
bytecode.enhancement.lazy.proxy.LazyOneToOneMappedByTest    :: AssertionFailedError (bare)
bytecode.enhancement.lazy.proxy.MergeDetachedToProxyTest    :: AssertionFailedError (bare)
bytecode.enhancement.lazy.proxy.QueryScrollingWithInheritanceProxyEagerManyToOneTest :: AssertionError (bare)
bytecode.enhancement.lazy.proxy.QueryScrollingWithInheritanceProxyTest :: AssertionError (bare)
bytecode.enhancement.lazy.proxy.inlinedirtychecking.DirtyCheckPrivateUnMappedCollectionTest :: RuntimeException: Could not build SessionFactory: Basic collection has element type '...' [truncated]
bytecode.enhancement.lazy.proxy.inlinedirtychecking.ManyToOnePropertyAccessByFieldTest :: AssertionError (bare)
bytecode.enhancement.lazy.proxy.inlinedirtychecking.ManyToOneWithEmbeddedAndNotOptionalFieldTest :: AssertionError (bare)
cache.LazyOneToOneWithCollectionTest                        :: AssertionFailedError (bare)
cache.ManyToOneTest                                         :: AssertionFailedError (bare)
cache.ManyToOneWithOptimisticLockingTest                    :: AssertionFailedError (bare)
cache.StructuredEntityCacheInheritanceTest                  :: AssertionFailedError (bare)
entitygraph.named.parsed.ClassLevelTests                    :: AssertionError (bare)
entitygraph.named.parsed.PackageLevelTests                  :: AssertionError (bare)
entitygraph.parser.EntityGraphParserTypedTest                :: AssertionError (bare)
fetchprofiles.EntityLoadedInTwoPhaseLoadTest                :: AssertionFailedError (bare)
filter.FilterWitSubSelectFetchModeTest                      :: AssertionError (bare)
function.array.ArrayPositionsTest                           :: AssertionFailedError: expected: java.util.ImmutableCollections$List12@... but was: java.util.ArrayList@...  (wrong collection impl type returned, not a value mismatch)
function.json.JsonArrayUnnestTest                           :: RuntimeException: Could not build SessionFactory: org.hibernate.mapping.Column cannot be cast to org.hibernate.mapping.AggregateColumn
graph.EntityGraphsTest                                       :: AssertionFailedError (bare)
hqlfetchscroll.QueryScrollingWithInheritanceTest             :: AssertionError (bare)
inheritance.InheritanceDeleteBatchTest                       :: AssertionError (bare)
inheritance.embeddable.EmbeddableInheritance2LCTest          :: AssertionFailedError (bare)
jpa.compliance.tck2_2.caching.InheritedCacheableTest         :: AssertionError (bare)
jpa.compliance.tck2_2.caching.SubclassOnlyCachingTests       :: AssertionError (bare)
jpa.lock.LockTest                                             :: AssertionFailedError: execution exceeded timeout of 5000 ms by 1239 ms  (performance/timing assertion, marginal)
keymanytoone.bidir.embedded.KeyManyToOneTest                  :: AssertionError (bare)
mapping.array.ArrayTests                                      :: AssertionError (bare)
mapping.collections.ElementCollectionCustomSqlMutationsTest   :: AssertionError (bare)
mapping.converted.converter.generics.ParameterizedAttributeConverterParameterTypeTest :: AssertionFailedError: expected: <List<String>> but was: <List<>>  (generic type-parameter erasure/reification gap)
mapping.fetch.depth.NoDepthTests                              :: PersistenceException: Unable to locate persistence units  (different symptom from the 2026-07-05/07 finding for this same class — see hib-nodepthtests-persistenceprovider-serviceloader-residual.md if still present, may need re-verification)
mapping.lazytoone.InstrumentedProxyLazyToOneTest               :: AssertionError (bare)
mapping.lazytoone.LazyToOneTest                                :: AssertionError (bare)
mapping.manytomany.ManyToManyCustomSqlMutationsTest            :: AssertionError (bare)
mapping.manytomany.ManyToManySQLJoinTableRestrictionTest       :: AssertionError (bare)
mapping.onetomany.OneToManyBidirectionalTest                   :: AssertionError (bare)
mapping.onetomany.OneToManyCustomSqlMutationsTest              :: AssertionError (bare)
mapping.onetomany.OneToManyEmptyCollectionTest                 :: AssertionError (bare)
mapping.type.format.XmlFormatterTest                           :: ClassCastException: java.lang.String cannot be cast to java.lang.Integer  (real type-confusion bug, worth a closer look on its own)
mapping.type.java.LocaleJavaTypeDescriptorTest                 :: AssertionFailedError: expected: <ja-u-> but was: <ja-u-nu-japanese>  (locale/ICU formatting difference)
multitenancy.discriminator.DiscriminatorMultiTenancyTest       :: AssertionFailedError (bare)
multitenancy.schema.CurrentTenantResolverMultiTenancyTest      :: AssertionFailedError (bare)
multitenancy.schema.SchemaBasedDataSourceMultiTenancyTest      :: AssertionFailedError (bare)
multitenancy.schema.SchemaBasedMultiTenancyTest                :: AssertionFailedError (bare)
ondelete.OnDeleteTest                                          :: AssertionError (bare)
ondemandload.LazyLoadingTest                                   :: AssertionError (bare)
proxy.concrete.ConcreteProxyTest                                :: AssertionError (bare)
service.ClassLoaderServiceImplTest                               :: AssertionError (bare)  (previously investigated 2026-07-05/07 this session, was intermittently PASS/FAIL — see prior findings if still present)
tool.schema.scripts.StatementsWithoutTerminalCharsImportFileTest :: AssertionFailedError: SqlScriptParserException expected  (expected exception not thrown — a validation gap)
write.staticinsert.SingleTableStaticInsertTests                  :: AssertionFailedError (bare)
write.staticinsert.SingleTableWithSecondaryTableStaticInsertTests :: AssertionFailedError (bare)
```

## Next steps (not yet done)

## 2026-07-12 focused investigation: Hibernate Models annotation-loss boundary

The `sql.exec.EmbeddedIdEntityTest` group is not a bare assertion failure. A
full-stack rerun on current `dev` fails all six methods with:

```
java.lang.IllegalArgumentException: Unknown entity type
'org.hibernate.testing.orm.domain.gambit.EmbeddedIdEntity'
```

The class is passed correctly through `@DomainModel(annotatedClasses = ...)`:
direct reflection reports one class, and direct `Class.getAnnotation(Entity.class)`,
`getDeclaredAnnotations()`, and `getAnnotations()` all expose `@Entity`.

The loss occurs inside Hibernate Models' `JdkClassDetails`. Its
`Class::getAnnotations` method-reference supplier yields no direct annotation
usages, so `MetadataSources.addAnnotatedClass(EmbeddedIdEntity.class)` builds
no entity binding. This is independent of JIT (`--nojit` reproduces it).

The VM's ordinary lambda dispatch and the interpreter's
`try_lambda_dispatch` path are distinct. The latter is the relevant path for
the Hibernate Models `invokedynamic` supplier. A direct `Supplier` probe for
the same `Class::getAnnotations` reference hangs, while the direct reflection
call succeeds. Do not treat annotation parsing, annotation-proxy shape, or
the `@DomainModel` class-literal array as the root cause: each was checked and
works in isolation. No runtime patch was retained because the attempted
Class-mirror owner-selection change did not alter the reproducer.

- Investigate the `InstantiationException` and `sql.exec.*` sub-patterns as
  their own mini-clusters (each has enough members to suggest a shared
  cause, just not yet enough evidence to write a confident root-cause
  hypothesis).
- For the true one-offs (bare `AssertionError`/`AssertionFailedError` with
  no message), get full stack traces — the harness captures a class name +
  optional message, and when Hibernate's own assertion has no message, this
  harness output alone is insufficient to even begin triage. A full JUnit
  XML report or `-DBG` full-output rerun would be needed.
- Confirm CratonVM-specificity with a HotSpot run (pending) for all of the
  above.
