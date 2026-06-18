# Hibernate dev-run hangs — cluster summary (rc=124 @ 600s, CV-only)

Census mode: fork-per-class, JIT-off, 600s per-class timeout. All classes below **PASS on HotSpot**
(JDK 25). Hangs are grouped by **confirmed** root cause (watchdog main-thread dump) or **inferred** (same
subsystem / signature). Confirmed via 120s watchdog thread dumps on a representative class.

> ⚠️ Caveat: the census ran under heavy CPU contention (a concurrent peer session). A few large classes may
> be *slow-not-hung* rather than truly deadlocked; the watchdog dumps below distinguish real stalls (stuck in
> one native/loop) from progress. Where a dump shows a real stall, it's a genuine hang.

## Cluster H1 — JAXB model-building slow / class-loading (⚠️ re-diagnosed — NOT a `retainAll` bug)
**Root (revised):** the `retainAll` framing was refuted — standalone `LinkedHashMap.keySet().retainAll`
is correct on CratonVM (all sizes, under GC). Live `cdb` shows the native activity is **class loading**
(`native_map_put → alloc_object → ensure_synthetic_class → ZipArchive::by_name → indexmap → hashbrown`)
during JAXB's reflection-heavy model building — i.e. interpreter-slow class-loading/reflection (and/or an
intermittent zip-index hot spot), not a localized collection loop. Deeper investigation, not a quick fix.
Full write-up + repros: [XML-jaxb-retainAll-infinite-hang.md](hibernate-jaxb-classloading-bytebuddy-bootstrap-slow.md).
**Confirmed:** `annotations.xml.ejb3.Ejb3XmlElementCollectionTest`.
**Inferred (same JAXB XML-binding path):** `Ejb3XmlManyToOneTest`, `Ejb3XmlOneToOneTest`,
`bootstrap.binding.annotations.access.xml.XmlAccessTest`, `boot.models.xml.XmlProcessingSmokeTests`,
`boot.jaxb.mapping.HbmTransformationJaxbTests`.

## Cluster H2 — ByteBuddy `MethodGraph` proxy-factory hang ✅ confirmed real stall
**Root:** stuck in `net.bytebuddy…MethodGraph$Compiler$Default.doAnalyze` (method-graph harmonization /
token `hashCode`) during `EntityRepresentationStrategyPojoStandard.instantiateProxyFactory` for a
`JoinedSubclassEntityPersister` — i.e. **entity proxy generation during SessionFactory bootstrap** never
finishes (the class never reaches its tests). Either an infinite MethodGraph recursion (cyclic type
hierarchy from CV reflection) or pathological ByteBuddy slowness on the interpreter. Needs investigation
(real-infinite vs severe-slow).
**Confirmed:** `hql.HQLTest` (7 Hibernate log lines then stuck in ByteBuddy at 120s).
**Inferred (complex-entity / joined-subclass / batch bootstrap):** `hql.ASTParserLoadingTest`,
`hql.BulkManipulationTest`, `joinedsubclassbatch.JoinedSubclassBatchingTest`,
`joinedsubclassbatch.IdentityJoinedSubclassBatchingTest`, `batch.BatchTest`,
`batchfetch.DynamicBatchFetchTest`, `jpa.criteria.InPredicateTest`, `bulkid.OracleInlineMutationStrategyIdTest`,
`id.enhanced.OptimizerConcurrencyUnitTest`,
`immutable.entitywithmutablecollection.inverse.VersionedEntityWithInverseOneToManyJoinFailureExpectedTest`,
`extendshbm.ExtendsTest`, `bootstrap.scanning.PackagedEntityManagerTest`.

## Cluster H3 — JTA / socket (Narayana) hang — documented
Same family as the JTA crash cluster. See
[JTA-txcontrol-clinit-gethostaddress-null.md](hibernate-jta-txcontrol-getinetaddress-per-class-report.md) and
[docs/known-issues/hibernate-jta-narayana-xa-completion-and-socket-loopback.md](hibernate-jta-narayana-xa-completion-and-socket-loopback.md).
**Classes:** `connections.ThreadLocalCurrentSessionTest` (and the `connections`/`transaction` crash classes
that hang rather than crash depending on which JTA platform/socket path is hit).

## Cluster H4 — JSON function (post-fix slow / unnest) — likely tied to JSON work
`function.json.JsonArrayUnnestTest` — JSON-function family (the 4 JSON SIGSEGV classes are fixed this run;
this one timed out rather than crashing). Re-check after the `al_state` fix.

## Environmental (NOT a CV-only bug)
`boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest` — **HotSpot also HANGs** on this class
(baseline status = HANG); excluded from CV-only counts.

---

### Status of clusters
| cluster | kind | status |
|---|---|---|
| H1 JAXB `retainAll` | real infinite loop | 🔴 open — fix-candidate (collection iterator) |
| H2 ByteBuddy `MethodGraph` | real stall (bootstrap proxy gen) | 🔴 open — investigate infinite-vs-slow |
| H3 JTA / socket | recovery/XA + socket loopback | 🔴 open — handoff (see known-issues doc) |
| H4 JSON unnest | likely interpreter-slow | ⚪ re-check post-fix |
| DefaultCatalogAndSchema | environmental (HS hangs too) | — excluded |
