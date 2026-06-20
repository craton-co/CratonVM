# Hibernate dev-run hangs — cluster summary (rc=124 @ 600s, CV-only)

> **Update 2026-06-20 — H1/H2/H3 RESOLVED (do-not-reproduce / fixed).** Re-verified against current `dev`
> with a built binary + programmatic bootstraps (`_hibrepro/HibBoot`, `HibJoined`, `XaProbe`):
> **H1** JAXB class-load storm is fixed on `dev` (`1db07c35`/`25c42e13`); **H2** ByteBuddy `MethodGraph`
> JoinedSubclass bootstrap completes in ~16s `--nojit` (no stall); **H3** JTA/socket = an `accept()`
> deadlock, fixed on branch `fix/hib-jta-xa-loopback`. See the per-cluster docs (now in `docs/internal/`).
> **H4 re-checked 2026-06-20 — STILL HANGS, and re-attributed: it is NOT a JSON-function bug.** Run via the
> JUnit launcher (`function.json.JsonArrayUnnestTest`, with `hibernate-community-dialects` added to the cp),
> the 120s watchdog pins the main thread in the **HQL/ANTLR parser** —
> `HqlParser.selectStatement → selectClause → selectionList → selection → selectExpression` — called from
> `JsonArrayUnnestTest.lambda$testUnnestOrdinality$0` (a test *query*, not bootstrap, not JSON-function
> rendering). All 6 watchdog samples show the same 5 frames ⇒ an HQL-parser infinite-loop-or-severe-slowness.
> Same area as the deferred ANTLR deep-recursion / interpreter-throughput cluster (cf. bug C
> `springrepos-extension-hang-jit-throughput-and-deep-recursion` and the "instance methods don't tier-up"
> note). 🔴 **OPEN** (deeper, separate from the resolved H1–H3).
> The real Hibernate JUnit launcher works on CratonVM —
> `JtaCustomAfterCompletionTest` passes end-to-end (the earlier `@ExtendWith` "blocker" was a misdiagnosed
> non-reproducing transient — see
> [`../internal/junit5-extendwith-meta-annotation-parameterresolver.md`](../internal/junit5-extendwith-meta-annotation-parameterresolver.md)).

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

## Cluster H4 — HQL/ANTLR parser hang in `testUnnestOrdinality` (🔴 OPEN — re-attributed, NOT JSON)
`function.json.JsonArrayUnnestTest` — re-checked 2026-06-20 on current `dev` via the JUnit launcher
(needs `hibernate-community-dialects` on the cp — `DialectFeatureChecks` statically references `TiDBDialect`;
without it the container fails fast with `NoClassDefFoundError`, which is a harness artifact, not the bug).
With the dialect on the cp it **hangs (rc=124)**. The 120s watchdog pins the main thread in the HQL parser
(`HqlParser.selectExpression` ← `selection` ← `selectionList` ← `selectClause` ← `selectStatement`), driven
from `JsonArrayUnnestTest.lambda$testUnnestOrdinality$0` — i.e. parsing a **test HQL query**, not bootstrap,
not the 4 JSON SIGSEGV classes (those are fixed) and not JSON-function SQL rendering. All 6 watchdog samples
catch the same 5 parser frames ⇒ an HQL-parser infinite-loop or severe interpreter slowness. This is the
**deferred ANTLR deep-recursion / interpreter-throughput cluster** (cf.
[springrepos-extension-hang-jit-throughput-and-deep-recursion.md](springrepos-extension-hang-jit-throughput-and-deep-recursion.md)
and the "instance methods don't invocation-tier-up" perf note), **not** a JSON defect.

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
| H4 `JsonArrayUnnestTest` | HQL/ANTLR parser hang in `testUnnestOrdinality` (NOT JSON) | 🔴 **OPEN** — re-confirmed 2026-06-20; deferred parser-throughput/deep-recursion cluster |
| DefaultCatalogAndSchema | environmental (HS hangs too) | — excluded |
