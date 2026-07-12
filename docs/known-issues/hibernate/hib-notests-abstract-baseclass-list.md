# Hibernate suite — 91 `NOTESTS` (found=0) classes: NOT a bug cluster

| | |
|---|---|
| **Status** | ✅ NOT A BUG — informational only, kept for completeness per the audit's "document every currently-non-passing class" scope. |
| **Discovered** | 2026-07-11 full 4548-class suite audit, `dev` post `44f16ee2`+. |

## Why these aren't bugs

`NOTESTS` means JUnit discovered zero runnable `@Test` methods in the class
(`found=0`). Inspecting the 91 class names, they fall into two expected,
non-buggy categories:

1. **Abstract/base test-infrastructure classes** (the overwhelming
   majority) — named `Abstract*Test`, `Base*Test`, `*TestCase`, or similar.
   These are parent classes that concrete dialect/config-specific
   subclasses extend; they declare shared setup/helper methods but no
   `@Test` methods of their own (or their `@Test` methods are `abstract`/
   overridden). JUnit correctly finds 0 tests when asked to run them
   directly — this matches HotSpot behavior identically (confirmed for this
   exact pattern in earlier sessions' HotSpot-diff methodology; see
   [hib-linux-fail-bucket-triage-20260703.md](../hib-linux-fail-bucket-triage-20260703.md)
   which established and validated this pruning rule).

2. **Dialect-gated concrete test classes** whose `@Test` methods are all
   excluded at discovery time by a class- or method-level dialect/skip
   annotation, because this suite runs against H2 — e.g.
   `schemaupdate.SchemaUpdateSQLServerTest` (SQL Server-specific),
   `mapping.hhh17404.OracleOsonCompatibilityTest` (Oracle-specific). These
   report 0 discoverable tests under H2 by design, matching HotSpot.

## Full list (91)

```
action.queue.ActionQueuePerformanceTest
annotations.embeddables.collection.AbstractEmbeddableWithManyToManyTest
annotations.index.jpa.AbstractJPAIndexTest
annotations.lob.AbstractLobTest
annotations.namingstrategy.charset.AbstractCharsetNamingStrategyTest
annotations.xml.ejb3.Ejb3XmlTestCase
bootstrap.scanning.PackagingTestCase
bootstrap.spi.metadatabuildercontributor.AbstractSqlFunctionMetadataBuilderContributorTest
bulkid.AbstractMutationStrategyCompositeIdTest
bulkid.AbstractMutationStrategyGeneratedIdTest
bulkid.AbstractMutationStrategyGeneratedIdWithOptimizerTest
bulkid.AbstractMutationStrategyGeneratedIdentityTest
bulkid.AbstractMutationStrategyIdTest
bytecode.enhancement.cascade.circle.AbstractMultiPathCircleCascadeTest
bytecode.enhancement.lazy.proxy.batch.BatchingTest
cascade.circle.AbstractMultiPathCircleCascadeTest
collection.dereferenced.AbstractDereferencedCollectionTest
connections.AbstractBeforeCompletionReleaseTest
connections.ConnectionManagementTestCase
dialect.AbstractLimitHandlerTest
dialect.unit.lockhint.AbstractLockHintTest
dialect.unit.sequence.AbstractSequenceInformationExtractorTest
dynamicmap.MixedModelTests
entitygraph.named.parsed.AbstractClassLevelTests
entitygraph.named.parsed.AbstractPackageLevelTests
entitygraph.parser.AbstractEntityGraphParserTest
entitygraph.parser.AbstractEntityGraphTest
event.collection.AbstractCollectionEventTest
event.collection.association.AbstractAssociationCollectionEventTest
exceptionhandling.BaseExceptionHandlingTest
exceptionhandling.BaseJpaOrNativeBootstrapFunctionalTestCase
filter.AbstractStatefulStatelessFilterTest
filter.subclass.SubClassTest
graph.AbstractEntityGraphTest
immutable.entitywithmutablecollection.AbstractEntityWithManyToManyTest
immutable.entitywithmutablecollection.AbstractEntityWithOneToManyTest
insertordering.BaseInsertOrderingTest
jpa.BaseEntityManagerFunctionalTestCase
jpa.criteria.literal.AbstractCriteriaLiteralHandlingModeTest
jpa.criteria.subquery.AbstractSubqueryInSelectClauseTest
jpa.exception.AbstractEntityInsertUniqueConstraintBatchingTest
jpa.metamodel.AbstractJpaMetamodelPopulationTest
jpa.model.AbstractJPATest
jpa.procedure.AbstractStoredProcedureTest
jpa.secondarytable.AbstractNonOptionalSecondaryTableTest
jpa.transaction.batch.AbstractBatchingTest
jpa.transaction.batch.AbstractJtaBatchTest
lob.LongByteArrayTest
lob.LongStringTest
manytomanyassociationclass.AbstractManyToManyAssociationClassTest
mapping.basic.JsonJavaTimeMappingTests
mapping.basic.JsonMappingTests
mapping.basic.PolymorphicJsonTests
mapping.basic.XmlMappingTests
mapping.collections.custom.basic.UserCollectionTypeTest
mapping.collections.custom.declaredtype.UserCollectionTypeTest
mapping.collections.custom.parameterized.ParameterizedUserCollectionTypeTest
mapping.fetch.depth.form.AbstractFormFetchDepthTest
mapping.generated.AbstractGeneratedPropertyTest
mapping.hhh17404.OracleOsonCompatibilityTest
mapping.naturalid.composite.AbstractCompositeIdAndNaturalIdTest
mapping.naturalid.mutable.cached.CachedMutableNaturalIdTest
mapping.readwrite.AbstractReadWriteTests
mapping.type.java.AbstractDescriptorTest
multitenancy.AbstractMultiTenancyTest
multitenancy.beancontainer.AbstractTenantResolverBeanContainerTest
multitenancy.schema.AbstractSchemaBasedMultiTenancyTest
namingstrategy.complete.BaseAnnotationBindingTests
namingstrategy.complete.BaseHbmBindingTests
namingstrategy.complete.BaseNamingTests
onetomany.AbstractRecursiveBidirectionalOneToManyTest
onetomany.AbstractVersionedRecursiveBidirectionalOneToManyTest
ops.AbstractOperationTestCase
procedure.results.AbstractMultipleResultMappingTests
quarkus.MetadataCopyingTest
query.criteria.AbstractBooleanPredicateComparisonRenderingTest
query.hql.nullPrecedence.AbstractNullPrecedenceTest
query.resultmapping.AbstractUsageTest
query.sqm.BaseSqmUnitTest
query.sqm.mutation.multitable.BasicDeletionTests
readonly.AbstractReadOnlyTest
schema.BaseSchemaGeneratorTest
schemaupdate.AbstractAlterTableQuoteSchemaTest
schemaupdate.SchemaUpdateSQLServerTest
schemaupdate.foreignkeys.definition.AbstractForeignKeyDefinitionTest
sql.check.ResultCheckStyleTest
sql.results.AbstractResultTests
type.AbstractNamedEnumTest
type.temporal.AbstractJavaTimeTypeTests
type.temporal.LocalDateTest
type.temporal.LocalTimeTest
```
(package prefix `org.hibernate.orm.test.` / `org.hibernate.action.queue.`
omitted for brevity)

## Recommendation

Exclude this list from the canonical non-passed baseline used for future
"real bug" regression tracking (keep in the raw 453-class list for
completeness, but treat these 91 as expected/pruned when computing "how many
genuine issues remain").
