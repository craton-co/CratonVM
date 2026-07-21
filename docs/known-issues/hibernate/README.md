# Hibernate ORM test suite — known issues index (2026-07-16 full-suite rerun)

Source: full 4548-class Hibernate ORM 8.0 test suite run on this local Windows
host, `dev` merged to `2f02e939d`, real-JDK, JIT on, 4 shards, `TIMEOUT=1200`.
Result: `PASS=4437 (97.6%) FAIL=10 HANG=1 CRASH=1 ABORTED=8 NOTESTS=91`. This is
a large improvement over the 2026-07-11 audit baseline (`PASS=4095, 90.0%`),
reflecting the large number of concurrent fix branches merged into `dev`
since (statistics-counter cluster, sql.exec cluster, enhancement/setter
cluster, ANTLR entity-graph NPE cluster, immutable-collection and
cascade-multipath hang clusters, connections/proxy serialization cluster,
boot-models XML/Jandex cluster, and the assertion-longtail catalog — see
`docs/internal/fixed-suite-bugs/` for the individual write-ups). Every
cluster from the 2026-07-11 audit is now archived there as FIXED/RESOLVED.

**Data-loss note:** the exact 2026-07-11 453-class and later 224-class
non-passed baselines (`apps/hib-suite-runner/nonpassed*.txt`) were lost to
local-disk-exhaustion file corruption discovered 2026-07-16 (511/683 files in
`apps/hib-suite-runner`, including `rerun.sh`, `common.args`,
`CratonRunner.java/.class`, and `testlist.txt`, were silently truncated to
zero bytes). The harness driver files were recovered from an intact
`hib-suite-runner.tar` backup (2026-06-28/29 vintage); the non-passed class
*lists* themselves were not in that backup and could not be recovered. This
full-suite rerun was run specifically to regenerate an accurate current
baseline rather than trust a stale/partial list.

The new 20-class non-passed list is saved as the canonical baseline at
`apps/hib-suite-runner/nonpassed.txt` (also `nonpassed20.txt`) for future
regression tracking — not committed (`apps/` is gitignored).

## Residual clusters (2026-07-21 "others" rerun)

- [storedproc-h2-inprocess-javac-classpath-scan-slowness-20260721.md](storedproc-h2-inprocess-javac-classpath-scan-slowness-20260721.md) — `sql.storedproc.{ResultMappingTest,StoredProcedureTest}` HANG (rc=124) and `sql.storedproc.StoredProcedureResultSetMappingTest`/`jpa.procedure.StoredProcedureResultSetMappingTest` FAIL with JUnit per-test/setup `TimeoutException` (120s). All four use H2's `CREATE ALIAS ... AS $$ ... $$` to compile a small Java stored-procedure body in-process via the real JDK's `javax.tools.JavaCompiler`. Root-caused (via a `--stack-dump-on-timeout` repro) to genuine, continuous CPU-bound forward progress deep inside real `javac`'s classpath scanner (`ClassFinder.scanUserPaths`/`JavacFileManager$ArchiveContainer.list`) walking the harness's ~241-jar test classpath to resolve one `import` — not a deadlock. Distinct from the already-fixed HIB-CV-27 `File.separator` compile-error bug (confirmed still fixed here). Reconciles two prior docs that flagged this same class pair as unexplained host-load-correlated flakiness — the underlying compile is inherently slow enough on CratonVM to sit at the edge of the 120s/300s timeouts, so host contention (present in abundance on this shared box) tips individual runs over one threshold or another. OPEN, not fixed; exact CratonVM-side hot loop responsible for the per-call overhead not yet isolated.
- [classloaderserviceimpltest-regressions-20260721.md](classloaderserviceimpltest-regressions-20260721.md) — BOTH `ClassLoaderServiceImplTest` classes (`org.hibernate.orm.test.service` and `org.hibernate.orm.test.bootstrap.registry.classloading`) FAIL again, each a **regression** of a HIB-CV-16/HIB-CV-24 bug fixed 2026-06-30 (`579c4836b`), each with a NEW symptom rather than a simple revert. (1) `service.ClassLoaderServiceImplTest.testStoppableClassLoaderService` (HHH-8363): the 14b fix's inline `file:` URL → filesystem-path logic in `native-builtins/src/service_loader.rs::discover_providers` mishandles Windows drive-letter paths (`file:/C:/...` → malformed `/C:/...`, `std::fs::read` fails with `ERROR_INVALID_NAME`), while the already-correct sibling helper `file_url_path_to_fs_path` goes unused at that call site — likely latent since original verification. (2) `bootstrap.registry.classloading.ClassLoaderServiceImplTest.testLookupBefore`: a custom loader's `loadClass(String)` override now runs TWICE (`expected:<1> but was:<2>`, was `expected:<1> but was:<0>` pre-fix) when reached via ordinary bytecode dispatch before its own `super.loadClass` reaches the native `cl_real_load_class`; caused by `invoke_single_load_class_override`'s reentrancy guard (`native-builtins/src/classloader.rs`), added 2026-07-18 by an unrelated Spring Boot fix (`5a7ccb810`), which can't tell "override already running via external caller" from "native-first entry, not yet dispatched". OPEN, not fixed.

## Residual clusters (this rerun)

- FIXED/RETIRED: [hib-120s-junit-timeout-cluster-20260716.md](../../internal/hib-120s-junit-timeout-cluster-20260716.md) — all original timeout classes and the linked `LockTest` residual pass in one real-JDK combined-run acceptance test.
- [hib-misc-residuals-20260716-FIXED.md](../../internal/fixed-suite-bugs/hib-misc-residuals-20260716-FIXED.md) — historical Hibernate residual investigation, now fully closed. The final `DefaultCatalogAndSchemaTest` BigInteger AIOOBE is quarantined at the JIT admission and background-scheduling boundaries; the 132-test class passes under both normal JIT and `--nojit`.

## Already tracked elsewhere (all FIXED/RESOLVED as of this rerun)

See `docs/internal/fixed-suite-bugs/` for the full archive of the 2026-07-11
audit clusters, all confirmed fixed or resolved by the time of this rerun:
statistics-counters-zero, bytecode-enhancement-propertyaccessexception,
entitygraph-antlr-rulenode-npe, immutable-entitywithmutablecollection-hang,
cascade-multipathcircle-hang, boot-models-xml-qname-jandex,
connections-proxy-serializationexception, generic-timeout-hang-longtail,
assertionfailederror-longtail-triage, proxyclassreuse-loader-blind-class-resolution.

- [hib-notests-abstract-baseclass-list.md](../../internal/hib-notests-abstract-baseclass-list.md) — 91 classes, NOT a bug (abstract base classes with 0 discoverable tests, matches HotSpot).
