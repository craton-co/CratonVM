# G1, `passed.txt` category: 63 classes regressed since the 2026-08-11 baseline

**Status: filed, not yet root-caused.** Investigation was stopped after confirming
these are real and grouping them by signature; no fix attempted.

## Context

`run-hib.sh --category passed --shards 4` was run under G1 (`-XX:+UseG1GC`, via a
wrapper script) on `dev` merged into `test/hib-local-0712-v3` as of 2026-08-14
(merge commit `cce82a1de` from `origin/dev`). `passed.txt` lists 4453 classes that
were previously green. This run recorded:

```
status: ABORTED=6 PASS=4381 HANG=1 FAIL=65  sum_class_ms=46867336
```

Run dir: `apps/hib-suite-runner/runs/g1-4shard-20260814-041902/run-20260814-041902-passed`.

## First pass: separating known issues from new regressions

Cross-referencing the 72 non-PASS classes against the 2026-08-11 Windows
real-HotSpot baseline (`runs/hotspot-baseline-20260811-100325`) found 9 that
were **already** failing/aborting there too — i.e. not new:

- `UniqueConstraintBatchingTest` (FAIL on both)
- `InheritedTest`, `MappedSuperclassTest` (bytecode-enhancement ABORTED cluster, both)
- `ManyToManyAssociationClassGeneratedIdTest` (ABORTED, both)
- `FunctionTests`, `StandardFunctionTests` (FAIL, both — known Windows+H2 flake, see 2026-08-11 findings)
- `InstantTests`, `LocalDateTimeTest`, `OffsetTimeTest` (ABORTED, both — known temporal-type JUnit-assumption cluster)

The remaining **63 classes passed cleanly on that same baseline** three days ago.

## Confirmed genuine (not a fluke)

Spot-checked 4 of the 63 (`CommentsTest`, `SequenceGeneratorTest`, `OneToOneTest`,
`DatabaseMultiTenancyTest`):

- Reproduce **deterministically** on rerun.
- Fail identically under **default GC too**, not just G1 — this is not a
  G1-specific bug, despite surfacing in the G1 run.
- **Real HotSpot, tested fresh right now** (not the stale baseline) still passes
  all 4 cleanly.

So this is a genuine, current CratonVM regression, introduced somewhere in the
three days of `dev` churn between the 2026-08-11 baseline and this merge. The
exact commit(s) were not bisected.

## Grouped by failure signature

Not one bug — at least 6 distinct clusters plus a long tail of singletons.
Listed by cluster size; the largest is the best lead if this gets picked back up.

### `RuntimeException`/`MappingException`: `param named "property" is required for foreign id generation strategy` — 13 classes

The single largest cluster. All fail during `SessionFactory` bootstrap, i.e.
metadata/annotation processing, not runtime query execution — points at the
`foreign` id-generator strategy's `@Parameter(name="property", ...)` reading.

- `annotations.onetoone.OneToOneTest`
- `annotations.onetoone.OptionalOneToOneMappedByTest`
- `boot.models.foreigngenerator.ForeignGeneratorTests`
- `hql.ASTParserLoadingTest`
- `hql.BulkManipulationTest`
- `hql.ScrollableCollectionFetchingTest`
- `hql.TreatKeywordTest`
- `hql.WithClauseTest`
- `jpa.cascade2.CascadeTest` (as `MappingException`, same message)
- `onetoone.cache.OneToOneCacheTest`
- `onetoone.cache.OneToOneConstrainedCacheTest`
- `onetoone.nopojo.DynamicMapOneToOneTest`
- `orphan.one2one.pk.bidirectional.DeleteOneToOneOrphansTest`

### `NullPointerException`: `Cannot invoke "org.hibernate.mapping.Table.getComment()"` — 4 classes

- `annotations.comment.CommentTest`, `CommentsTest`
- `annotations.comment.jpa.CommentTest`, `CommentsTest`

Table comment metadata is apparently never populated; the test's own assertion
NPEs reading it back.

### `SQLGrammarException`: table not found (multi-tenant schema never created) — 7 classes

- `bootstrap.scanning.PackagedEntityManagerTest`
- `jpa.compliance.tck2_2.caching.CachingWithSecondaryTablesTests`
- `multitenancy.DatabaseMultiTenancyTest`, `DatabaseTimeZoneMultiTenancyTest`, `SchemaMultiTenancyTest`
- `multitenancy.beancontainer.MultiTenantConnectionProviderFromBeanContainerTest`, `MultiTenantConnectionProviderFromSettingsOverBeanContainerTest`

Per-tenant/per-connection DDL apparently isn't running before the test inserts.

### `NullPointerException`: `Cannot read field "table" because "this.map" is null` — 5 classes

- `schemaupdate.QuotedTableNameWithForeignKeysSchemaUpdateTest`
- `schemaupdate.SchemaUpdateTest`
- `schemaupdate.foreignkeys.SchemaUpdateWithKeywordAutoQuotingEnabledTest`
- `schemaupdate.foreignkeys.crossschema.CrossSchemaForeignKeyGenerationTest`
- `tool.schema.internal.CheckForExistingForeignKeyTest`

All in schema-update tooling; likely the same root cause as the next cluster
(foreign-key/table metadata not populated the way schema-update code expects).

### `NullPointerException`: `Cannot invoke "Table.getForeignKeyCollection()"` — 2 classes

- `foreignkeys.disabled.DefaultConstraintModeTest`
- `foreignkeys.disabled.OneToManyBidirectionalForeignKeyTest`

### `TimeoutException` (120s) — 3 classes

- `batch.BatchTest` (`testBatchInsertUpdate`)
- `batchfetch.DynamicBatchFetchTest` (`testMultiLoad`)
- `sql.exec.SmokeTests` (`testQueryConcurrency`)

Worth checking whether these are genuine hangs or just slow under load on this
shared host — not confirmed either way.

### Ungrouped singletons — 29 classes

Assorted `AssertionError`/`AssertionFailedError`/`MappingException` failures
without an obvious shared signature, several in the `idgen.enhanced.*` family
(sequence/table generators — plausibly related to the `property`-param cluster
above, but not confirmed):

`annotations.id.generationmappings.NewGeneratorMappingsTest`,
`annotations.onetoone.OptionalOneToOnePKJCTest`, `boot.cfgXml.CfgXmlParsingTest`,
`cdi.lifecycle.ExtendedBeanManagerNotAvailableDuringTypeResolutionTest`,
`connections.ConnectionProviderFromBeanContainerTest`,
`hql.HqlParserMemoryUsageTest`, `id.GenericGeneratorTest`,
`id.SequenceGeneratorTest`, `id.uuid.strategy.CustomStrategyTest`,
`idgen.enhanced.forcedtable.BasicForcedTableSequenceTest`,
`idgen.enhanced.forcedtable.HiLoForcedTableSequenceTest`,
`idgen.enhanced.forcedtable.PooledForcedTableSequenceTest`,
`idgen.enhanced.sequence.BasicSequenceTest`,
`idgen.enhanced.sequence.HiLoSequenceMismatchStrategyTest`,
`idgen.enhanced.sequence.HiLoSequenceTest`,
`idgen.enhanced.table.BasicTableTest`, `idgen.enhanced.table.HiLoTableTest`,
`jpa.boot.PersistenceConfigurationTests`, `mapping.basic.ExplicitTypeTest`,
`mapping.basic.bitset.MetaUserTypeTest`,
`mapping.collections.custom.parameterized.ParameterizedUserCollectionTypeHbmVariantTest`,
`mapping.usertypes.UserTypeMappingTest`,
`multitenancy.discriminator.DiscriminatorMultiTenancyTest`,
`multitenancy.schema.CurrentTenantResolverMultiTenancyTest`,
`multitenancy.schema.SchemaBasedDataSourceMultiTenancyTest`,
`multitenancy.schema.SchemaBasedMultiTenancyTest`,
`schemaupdate.idbag.IdBagSequenceTest`,
`schemaupdate.idgenerator.SequenceGeneratorIncrementTest`,
`subselect.SubselectTest`

## Next steps (not started)

- Bisect `dev` between the 2026-08-11 baseline tip and `cce82a1de` to find the
  commit(s) responsible for each cluster — not attempted, would take a while
  given the size of the range.
- The `property`-param cluster (13 classes) is the highest-value lead: same
  message, same bootstrap-time failure point, across otherwise-unrelated test
  areas.
- The `this.map is null` / `getForeignKeyCollection()` clusters (7 classes
  combined) look like they could share a root cause with each other.
- Confirm whether the 3 `TimeoutException`s are genuine hangs or host-load
  artifacts (rerun in isolation, off-peak).
