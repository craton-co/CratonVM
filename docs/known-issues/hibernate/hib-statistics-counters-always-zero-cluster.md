# Hibernate suite — Statistics/counter subsystem reads back 0: the single largest cluster in this audit (94 classes)

| | |
|---|---|
| **Status** | 🔴 OPEN — 94 classes (35% of all FAIL, 21% of the entire 453-class non-passed list). Single highest-value finding in this audit. HotSpot confirmation pending, but the "always exactly 0 regardless of real activity" shape is not the kind of thing that would coincidentally also happen on HotSpot. |
| **Discovered** | 2026-07-11 full 4548-class suite audit, `dev` post `44f16ee2`+, while triaging the ~204-class "AssertionFailedError longtail". |
| **Area** | `org.hibernate.stat.spi.StatisticsImplementor` / `SessionFactory.getStatistics()` — second-level cache, query-cache, and persistence-operation counters. |

## Why this is one bug, not 94

Superficially these are 94 distinct test classes each with their own
"different" assertion failure — that's how they first appeared while
triaging the long tail of scattered `AssertionFailedError`s. But grouping by
signature shape reveals **every one of them is the identical pattern**: an
assertion that some Hibernate `Statistics` counter equals a small positive
integer (`1`, `2`, `3`, `4`, `10`, `12`, `20`, `40` — whatever the specific
test expects for its specific scenario) instead reads back **exactly `0`**:

```
org.opentest4j.AssertionFailedError: expected: <1> but was: <0>
org.opentest4j.AssertionFailedError: expected: <10> but was: <0>
org.opentest4j.AssertionFailedError: expected: <40> but was: <0>
java.lang.AssertionError: unexpected insert count
java.lang.AssertionError: unexpected delete counts
org.opentest4j.AssertionFailedError: unexpected execution count ==> expected: <1> but was: <0>
org.opentest4j.AssertionFailedError: NaturalId Cache Misses ==> expected: <1> but was: <0>
org.opentest4j.AssertionFailedError: Cache put should be one after insert ==> expected: <1> but was: <0>
org.opentest4j.AssertionFailedError: query is not considered as isImmutableNaturalKeyLookup, despite fullfilling all conditions ==> expected: <1> but was: <0>
```

The counter being checked varies by test (2nd-level cache hit/miss/put
counts, query-cache hit/miss counts, entity insert/update/delete counts,
query execution counts, natural-id cache counts, flush counts, statement
counts) but the failure mode is identical in every case: **the counter is
stuck at its initial value of 0**, as if the statistics subsystem is never
being incremented at all, regardless of what real database/cache activity
actually happened (and it clearly did happen — these are integration tests
that persist/query/flush real entities against H2 first, then check the
counter).

## Affected classes (94)

```
annotations.InMemoryTimestampGenerationBatchTest
annotations.collectionelement.OnDeleteCascadeToElementCollectionTest
annotations.onetomany.OneToManyNonPrimaryKeyJoinTest
annotations.query.QueryAndSQLTest
any.fetch.JoinFetchAnyQueryCacheTest
bytecode.enhancement.lazy.cache.UninitializedAssociationsInCacheTest
bytecode.enhancement.lazy.cache.UninitializedLazyBasicCacheTest
bytecode.enhancement.lazy.fetch.EnhancedFetchTest
bytecode.enhancement.lazy.fetch.FetchTest
bytecode.enhancement.lazy.proxy.BatchFetchProxyTest
bytecode.enhancement.lazy.proxy.BidirectionalProxyTest
bytecode.enhancement.lazy.proxy.DeepInheritanceProxyTest
bytecode.enhancement.lazy.proxy.DeepInheritanceWithNonEntitiesProxyTest
bytecode.enhancement.lazy.proxy.LazyGroupWithInheritanceAllowProxyTest
bytecode.enhancement.lazy.proxy.LazyGroupWithInheritanceTest
bytecode.enhancement.lazy.proxy.LazyToOnesNoProxyFactoryWithSubclassesStatefulTest
bytecode.enhancement.lazy.proxy.LazyToOnesNoProxyFactoryWithSubclassesStatelessTest
bytecode.enhancement.lazy.proxy.LazyToOnesProxyMergeWithSubclassesTest
bytecode.enhancement.lazy.proxy.LazyToOnesProxyWithSubclassesStatelessTest
bytecode.enhancement.lazy.proxy.LazyToOnesProxyWithSubclassesTest
bytecode.enhancement.lazy.proxy.LoadANonExistingEntityTest
bytecode.enhancement.lazy.proxy.LoadANonExistingNotFoundBatchEntityTest
bytecode.enhancement.lazy.proxy.LoadANonExistingNotFoundEntityTest
bytecode.enhancement.lazy.proxy.LoadANonExistingNotFoundLazyBatchEntityTest
bytecode.enhancement.lazy.proxy.MapsIdProxyBidirectionalTest
bytecode.enhancement.lazy.proxy.MapsIdProxyUnidirectionalTest
bytecode.enhancement.lazy.proxy.SimpleUpdateWithLazyLoadingWithCollectionInDefaultFetchGroupFalseTest
bytecode.enhancement.lazyCache.InitFromCacheTest
bytecode.enhancement.ondemandload.OnDemandLoadTest
bytecode.enhancement.ondemandload.OnDemandLoadWithCollectionInDefaultFetchGroupFalseTest
cache.CacheModeRefreshSessionTest
cache.CacheRegionStatisticsTest
cache.CollectionCacheEvictionComplexIdTest
cache.EnhancedProxyCacheTest
cache.EntityUpdateCacheModeIgnoreTest
cache.L2CacheAccessNoCommitTest
cache.NonstrictReadWriteMinimalPutsTest
cache.ReadOnlyMinimalPutsTest
cache.ReadWriteMinimalPutsTest
cache.SharedDomainDataAndQueryResultsTest
component.basic.ComponentTest
fetchprofiles.CollectionLoadedInTwoPhaseLoadTest
fetchprofiles.join.JoinFetchProfileTest
id.array.PrimitiveByteArrayIdCollectionTest
jpa.EntityManagerTest
jpa.compliance.tck2_2.caching.CachingWithSecondaryTablesTests
jpa.graphs.LoadAndFetchGraphTest
jpa.ops.MergeTest
jpa.ops.PersistTest
jpa.query.CachedQueryDirectReferenceTest
jpa.query.CachedQueryShallowCollectionNestedJoinFetchTest
jpa.query.CachedQueryShallowCollectionTest
jpa.query.CachedQueryShallowPolymorphicTest
jpa.query.CachedQueryShallowTest
jpa.query.CachedQueryShallowWithCollectionTest
jpa.query.CachedQueryShallowWithDiscriminatorBytecodeEnhancedTest
jpa.query.CachedQueryShallowWithDiscriminatorPolymorphicTest
jpa.query.CachedQueryShallowWithJoinFetchEagerTest
jpa.query.CachedQueryShallowWithJoinFetchLazyTest
jpa.query.CachedQueryTest
jpa.transaction.FlushAndTransactionTest
keymanytoone.bidir.component.EagerCollectionLazyKeyManyToOneTest
keymanytoone.bidir.component.EagerKeyManyToOneTest
keymanytoone.bidir.component.LazyKeyManyToOneTest
loading.multiLoad.FindMultipleFromCacheTest
loading.multiLoad.MultiLoadTest
mapping.fetch.subselect.SubselectFetchCollectionFromBatchTest
mapping.naturalid.NaturalIdTest
mapping.naturalid.immutable.ImmutableNaturalIdTest
mapping.naturalid.immutable.ImmutableNaturalKeyLookupTest
mapping.naturalid.immutableentity.ImmutableEntityNaturalIdTest
mapping.naturalid.mutable.MutableNaturalIdTest
mapping.naturalid.mutable.cached.CachedMutableNaturalIdNonStrictReadWriteTest
mapping.naturalid.mutable.cached.CachedMutableNaturalIdStrictReadWriteTest
ondeletecascade.OnDeleteCascadeRemoveTest
onetoone.joined.JoinedSubclassOneToOneTest
onetoone.singletable.DiscrimSubclassOneToOneTest
ops.CreateTest
ops.DeleteTest
ops.OneToManyMappedByCascadeDeleteTest
ops.SimpleOpsTest
pc.MultiLoadIdTest
propertyref.basic.PropertyRefTest
querycache.QueryCacheJoinFetchTest
querycache.QueryCacheTest
readonly.ReadOnlyNamedQueryTest
sql.hand.query.NativeSQLQueriesTest
stat.internal.ConcurrentQueryStatisticsTest
stat.internal.QueryPlanCacheStatisticsTest
stateless.GetMultipleFromCacheTest
stateless.StatelessSessionStatisticsTest
stats.StatsTest
timestamp.TimestampTest
uniquekey.NaturalIdCachingTest
```

Note: several classes with counter-adjacent but distinctly-worded messages
that are almost certainly the **same bug** were kept in the general longtail
doc rather than double-counted here, since their exact wording didn't match
the grep pattern used to build this list mechanically (e.g. any class whose
assertion message doesn't literally contain "expected: <N> but was: <0>" or
"unexpected {insert,delete,execution} count") — a manual pass over
[hib-assertionfailederror-longtail-triage.md](hib-assertionfailederror-longtail-triage.md)
may surface more members of this same cluster.

## Root-cause hypothesis (not yet confirmed, but narrow)

Given the sheer breadth (2nd-level cache stats, query-cache stats,
insert/update/delete counts, query execution counts, natural-id cache
stats, flush counts — essentially *every* counter category Hibernate's
`Statistics` interface exposes) and the *uniform* "stuck at exactly 0"
shape (not off-by-one, not occasionally-wrong — always precisely the
initial/zero value), the most likely explanations, roughly in order of
plausibility:

1. **The statistics-collector object itself isn't being incremented.**
   Hibernate's `StatisticsImpl` uses `java.util.concurrent.atomic.LongAdder`/
   `AtomicLong` fields internally for each counter, incremented at dozens of
   call sites throughout the persistence-execution pipeline
   (`SessionFactoryImpl`, `StatefulPersistenceContext`, the second-level
   cache access strategies, etc.). If CratonVM's dispatch to
   `SessionFactory.getStatistics()` returns a **different `StatisticsImpl`
   instance** than the one actually being incremented during persistence
   operations (e.g. a stale/duplicate object from a loader-identity or
   dual-classloader split — a pattern seen repeatedly elsewhere in this
   audit and this session generally), every read would show 0 even though
   increments are happening on some other, unreachable copy.
2. **`LongAdder`/`AtomicLong` increment itself is a no-op or broken** for
   this specific class under CratonVM (less likely given `AtomicLong` is
   fundamental and heavily exercised elsewhere without issue, but worth
   ruling out directly).
3. **Statistics collection is gated OFF** by a config flag CratonVM handles
   differently (e.g. `hibernate.generate_statistics` not being read/applied
   correctly) — but this seems less likely given `common.linux.args`'s
   config would need to differ specifically for CratonVM's property-reading
   path, and the tests' `@SessionFactory`/`@ServiceRegistry` annotations
   typically request statistics explicitly per-test.

Given (1) matches an already-well-established bug family in this codebase
(loader/dual-classloader-identity splits causing "the object being read is
not the object being written"), it's the leading hypothesis.

## Repro

Azure host harness — `cache.CacheRegionStatisticsTest` is a small, focused
class good for a first repro:
```bash
echo org.hibernate.orm.test.cache.CacheRegionStatisticsTest > /tmp/one.txt
<cratonvm> --java-home <jdk25> --Xmx 1500m @common.linux.args -Dcraton.batch=1 CratonRunner /tmp/one.txt 0
```

## Next steps (not yet done)

- Pick one small, simple failing test (e.g. `ops.SimpleOpsTest`'s "unexpected
  insert count") and instrument/trace: does `Statistics.getEntityInsertCount()`
  read from the SAME object instance that `SessionFactoryImpl`'s internal
  increment call sites write to? Compare object identity (`identityHashCode`)
  of the `Statistics`/`StatisticsImpl` at increment time vs. at read time.
- If identity diverges, this is the same loader/dispatch-identity family as
  other bugs in this codebase — find where the divergence is introduced
  (likely `SessionFactory.getStatistics()`'s dispatch, or how the
  `StatisticsImplementor` service gets registered/looked-up via
  CratonVM's service-registry native support).
- If identity is consistent, instrument the actual increment call sites
  (`StatisticsImpl.insertCount.increment()` etc.) to confirm they're even
  being reached — if not, the gap is earlier (an event/listener not firing).
- Confirm CratonVM-specificity with a HotSpot run (pending) — given the
  precision and uniformity of "always exactly 0", a HotSpot false-positive
  match across 94 unrelated classes would be extraordinarily unlikely; this
  cluster should be treated as effectively confirmed CratonVM-specific even
  before the HotSpot run completes.
- **This is the highest-value fix target in this entire audit** — a single
  root cause here would likely flip ~90+ classes from FAIL to PASS in one
  fix, more than every other cluster in this audit combined.
