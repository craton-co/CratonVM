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

## Residual clusters (this rerun)

- [hib-120s-junit-timeout-cluster-20260716.md](hib-120s-junit-timeout-cluster-20260716.md) — 7 classes, all failing with `TimeoutException ... timed out after 120 seconds` against Hibernate's own internal per-test `@Timeout(120)` JUnit annotation (not the outer harness timeout). One systemic cause suspected rather than 7 unrelated bugs — needs a HotSpot timing comparison to confirm CratonVM-specific slowness vs. genuine flakiness.
- [hib-misc-residuals-20260716.md](hib-misc-residuals-20260716.md) — remaining 13 non-passed classes not in the timeout cluster: `DefaultCatalogAndSchemaTest` (HANG, recurrence of a previously-investigated flaky class), `JarVisitorTest` (CRASH, rc=0/ms=0 harness-artifact shape), `LockTest` (real timing-sensitive AssertionFailedError), `CriteriaBuilderNonStandardFunctionsTest` (real `ConstraintViolationException`, not a timeout), `ManyToManyAssociationClassGeneratedIdTest` (new ABORTED entry, not previously catalogued), and the 6 already-known-expected ABORTED entries (`type.temporal.*` x5, `bytecode.enhancement.basic.*` x2 minus the new one) matching HotSpot per prior audits. **Update 2026-07-16 (follow-up session):** `ZonedDateTimeTest`/`LocalDateTimeTest`'s AQS `ConditionNode` stale-pointer livelock (a native `Executors.*` factory GC-relocation bug, not a root-scanning gap) is now FIXED — see [hib-aqs-threadpoolexecutor-relocation-livelock-FIXED.md](../../internal/fixed-suite-bugs/hib-aqs-threadpoolexecutor-relocation-livelock-FIXED.md). Both classes now complete solo; `ZonedDateTimeTest` surfaced a new, separate, still-OPEN residual (63/608 timezone-offset `AssertionFailedError`s not present on HotSpot) once it could finally run to completion. **Update 2026-07-16 (this session):** `DefaultCatalogAndSchemaTest`'s GC-corruption/discovery-crash family (the `found=0` `ClassCastException`/`AbstractMethodError`/`NullPointerException` shapes) is now CLOSED -- fixed by a `native-builtins/src/generics.rs` reflection-array GC-safety sweep (`0a1ec47b`) plus a concurrent session's `gc/src/gen_heap.rs` young-GC forwarding-walk fix (see [wildfly-cce0079-young-start-set-truncation-FIXED.md](../../internal/fixed-suite-bugs/wildfly-cce0079-young-start-set-truncation-FIXED.md)). The class now discovers all 132 tests (up from 0) with zero corruption signatures, but does NOT pass yet: it exposes a new, distinct, previously-hidden `ArrayIndexOutOfBoundsException`/`InvalidMappingException` bug (106/132 failures), tracked as a fresh OPEN item in [hib-misc-residuals-20260716.md](hib-misc-residuals-20260716.md#update-2026-07-17-this-session-aioobeinvalidmappingexception-not-independently-reproduced-across-60132-real-executions-new-severe-whole-class-discovery-performance-cliff-found-blocking-full-confirmation). **Update 2026-07-17:** `ZonedDateTimeTest`'s 63/608 timezone-offset residual root-caused to a `ZoneId.systemDefault()` host-timezone-leak native bug (ignored every `TimeZone.setDefault(...)` call) — FIXED, 63->20/608 failures; the remaining 20 are a distinct, narrower pre-1911 `Europe/Paris` Local-Mean-Time offset precision gap, still OPEN — see [hib-zoneddatetime-systemdefault-host-timezone-leak-FIXED.md](../../internal/fixed-suite-bugs/hib-zoneddatetime-systemdefault-host-timezone-leak-FIXED.md) and the residuals doc's 2026-07-17 update section. **Update 2026-07-17 (this session):** the 106/132 AIOOBE/InvalidMappingException failures could NOT be independently reproduced on a later dev tip (732241c8) -- 60/132 real parameterized executions across 5 of the 11 @Test methods (entityPersister, createSchema_fromSessionFactory, updateSchema_fromSessionFactory, tableGenerator, sequenceGenerator) all pass cleanly with zero occurrences, most likely fixed incidentally by later unrelated GC/reflection correctness work. A NEW, separate, severe performance cliff was found instead: whole-class (DiscoverySelectors.selectClass, what the harness actually uses) discovery+execution never completed even test #0 in up to 11 minutes across 3 attempts, despite the same work completing in 60-140s per method when split via selectMethod -- tracked as a fresh OPEN item, not yet root-caused to a specific line, in the same doc section. **Update 2026-07-17 (follow-up session):** the remaining 20/608 `ZonedDateTimeTest` pre-1911 `Europe/Paris` Local-Mean-Time residual noted above is now FIXED -- `found=608 ok=404 failed=0 aborted=204`, matching HotSpot exactly; root cause was the legacy `java.util.TimeZone`/`Calendar` path (not `java.time`, which was already correct), see [hib-paris-lmt-precision-FIXED.md](../../internal/fixed-suite-bugs/hib-paris-lmt-precision-FIXED.md).

## Already tracked elsewhere (all FIXED/RESOLVED as of this rerun)

See `docs/internal/fixed-suite-bugs/` for the full archive of the 2026-07-11
audit clusters, all confirmed fixed or resolved by the time of this rerun:
statistics-counters-zero, bytecode-enhancement-propertyaccessexception,
entitygraph-antlr-rulenode-npe, immutable-entitywithmutablecollection-hang,
cascade-multipathcircle-hang, boot-models-xml-qname-jandex,
connections-proxy-serializationexception, generic-timeout-hang-longtail,
assertionfailederror-longtail-triage, proxyclassreuse-loader-blind-class-resolution.

- [hib-notests-abstract-baseclass-list.md](../../internal/hib-notests-abstract-baseclass-list.md) — 91 classes, NOT a bug (abstract base classes with 0 discoverable tests, matches HotSpot).
