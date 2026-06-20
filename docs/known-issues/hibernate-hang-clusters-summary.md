# Hibernate dev-run hangs — cluster summary (rc=124 @ 600s, CV-only)

> **Update 2026-06-20 — H1/H2/H3 RESOLVED (do-not-reproduce / fixed).** Re-verified against current `dev`
> with a built binary + programmatic bootstraps (`_hibrepro/HibBoot`, `HibJoined`, `XaProbe`):
> **H1** JAXB class-load storm is fixed on `dev` (`1db07c35`/`25c42e13`); **H2** ByteBuddy `MethodGraph`
> JoinedSubclass bootstrap completes in ~16s `--nojit` (no stall); **H3** JTA/socket = an `accept()`
> deadlock, fixed on branch `fix/hib-jta-xa-loopback`. See the per-cluster docs (now in `docs/internal/`).
> H4 (JSON unnest) was not re-checked. The original JUnit repro *classes* still can't run via the launcher
> due to a separate `@ExtendWith` meta-annotation gap — see
> [`../known-issues/junit5-extendwith-meta-annotation-parameterresolver.md`](../known-issues/junit5-extendwith-meta-annotation-parameterresolver.md).

Census mode: fork-per-class, JIT-off, 600s per-class timeout. All classes below **PASS on HotSpot**
(JDK 25). Hangs are grouped by **confirmed** root cause (watchdog main-thread dump) or **inferred** (same
subsystem / signature). Confirmed via 120s watchdog thread dumps on a representative class.

> ⚠️ Caveat: the census ran under heavy CPU contention (a concurrent peer session). A few large classes may
> be *slow-not-hung* rather than truly deadlocked; the watchdog dumps below distinguish real stalls (stuck in
> one native/loop) from progress. Where a dump shows a real stall, it's a genuine hang.

## Cluster H1 — JAXB model-building slow / class-loading (✅ FIXED on dev — NOT a `retainAll` bug)
**Root (revised):** the `retainAll` framing was refuted — standalone `LinkedHashMap.keySet().retainAll`
is correct on CratonVM (all sizes, under GC). Live `cdb` shows the native activity is **class loading**
(`native_map_put → alloc_object → ensure_synthetic_class → ZipArchive::by_name → indexmap → hashbrown`)
during JAXB's reflection-heavy model building — i.e. interpreter-slow class-loading/reflection (and/or an
intermittent zip-index hot spot), not a localized collection loop. Deeper investigation, not a quick fix.
Full write-up + repros: [hibernate-jaxb-classloading-bytebuddy-bootstrap-slow.md](../internal/hibernate-jaxb-classloading-bytebuddy-bootstrap-slow.md) (now in `docs/internal/`).
**Confirmed:** `annotations.xml.ejb3.Ejb3XmlElementCollectionTest`.
**Inferred (same JAXB XML-binding path):** `Ejb3XmlManyToOneTest`, `Ejb3XmlOneToOneTest`,
`bootstrap.binding.annotations.access.xml.XmlAccessTest`, `boot.models.xml.XmlProcessingSmokeTests`,
`boot.jaxb.mapping.HbmTransformationJaxbTests`.

## Cluster H2 — ByteBuddy `MethodGraph` proxy-factory hang (✅ does-not-reproduce on current dev)
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

## Cluster H3 — JTA / socket (Narayana) hang (✅ FIXED — `accept()` deadlock)
Same family as the JTA crash cluster. See (now in `docs/internal/`)
[hibernate-jta-txcontrol-getinetaddress-per-class-report.md](../internal/hibernate-jta-txcontrol-getinetaddress-per-class-report.md) and
[hibernate-jta-narayana-xa-completion-and-socket-loopback.md](../internal/hibernate-jta-narayana-xa-completion-and-socket-loopback.md).
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
| H1 JAXB class-load storm | class-loading rescan storm (NOT `retainAll`) | ✅ **fixed on dev** (`1db07c35`/`25c42e13`) |
| H2 ByteBuddy `MethodGraph` | bootstrap proxy gen | ✅ **does-not-reproduce** — JoinedSubclass boots ~16s `--nojit` |
| H3 JTA / socket | `accept()` deadlock (NOT loopback-pairing) | ✅ **fixed** on branch `fix/hib-jta-xa-loopback` (`e0426050`) |
| H4 JSON unnest | likely interpreter-slow | ⚪ not re-checked |
| DefaultCatalogAndSchema | environmental (HS hangs too) | — excluded |
