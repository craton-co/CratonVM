# Hibernate ORM test suite — known issues index (2026-07-11 full-suite audit)

Source: full 4548-class Hibernate ORM 8.0 test suite run on the Azure remote
host (`20.83.144.174`), `dev` merged to `44f16ee2`+ (see individual docs for
exact commits), real-JDK, JIT on, 4 shards, `TIMEOUT=300`. Result:
`PASS=4095 (90.0%) FAIL=267 HANG=90 NOTESTS=91 ABORTED=5 CRASH=0`. Zero
crashes suite-wide — a strong signal the recent JIT `getfield` SIGSEGV
regression fix ([hib-global-temptable-nondeterministic-sigsegv-20260710-RESOLVED.md](../../internal/fixed-suite-bugs/hib-global-temptable-nondeterministic-sigsegv-20260710-RESOLVED.md))
holds at full scale.

The 453-class non-passed list is saved as the new canonical baseline at
`apps/hib-suite-runner/nonpassed.txt` (also `nonpassed453.txt`) for future
regression tracking — not committed (`apps/` is gitignored).

A HotSpot baseline comparison on the same 453 classes was started
(`out-hotspot453-20260711-231558` on the remote host) but ran very slowly
under heavy concurrent host load; docs below note where HotSpot confirmation
is still pending vs. already checked. Clusters whose symptom is a real
Java-level dispatch/reflection/classloading error specific to CratonVM's
implementation (not a Hibernate/H2 semantic gap) are treated as
CratonVM-specific by inspection even without a completed HotSpot run, since
these error shapes are not the kind of thing that would ever reproduce on a
correct JVM.

## Clusters (this audit), ranked by class count

- [hib-bytecode-enhancement-propertyaccessexception-setter-cluster.md](hib-bytecode-enhancement-propertyaccessexception-setter-cluster.md) — 19 classes. Loader-faithful bytecode-enhancement setter/reflection gap.
- [hib-entitygraph-antlr-rulenode-npe-cluster.md](hib-entitygraph-antlr-rulenode-npe-cluster.md) — 14 classes. ANTLR parse-tree NPE in legacy entity-graph string-syntax parsing.
- [hib-immutable-entitywithmutablecollection-hang-cluster.md](hib-immutable-entitywithmutablecollection-hang-cluster.md) — 17 classes (+`ImmutableTest`), all HANG, 100% hit rate. Immutable entity + mutable collection interaction.
- [hib-cascade-multipathcircle-hang-cluster.md](hib-cascade-multipathcircle-hang-cluster.md) — 12 classes, all HANG, 100% hit rate. Circular-cascade save/delete graph.
- [hib-misc-singleton-failures.md](hib-misc-singleton-failures.md) — ~12 classes across several small independent clusters (`InvalidMappingException` XML-mapping parse, `SyntaxException` HQL boolean-negation, `SQLGrammarException` x2 including a possible in-process-javac regression, `UnknownNamedQueryException`, `CannotContainSubGraphException`, `FailureExpectedExtension$ExpectedFailureDidNotFail` — the latter is not a defect). The four serialization EOF residuals are fixed and archived with the related connection/proxy cluster.
- [hib-generic-timeout-hang-longtail.md](hib-generic-timeout-hang-longtail.md) — 61 scattered HANG classes not in the two dedicated hang clusters. Likely a mix of genuine slowness and host-load artifacts; this session has repeatedly observed several of these flip between PASS/FAIL/HANG/CRASH across reruns — not individually triaged, needs a quiet-host recheck.
- [hibernate-assertionfailederror-longtail-triage-FIXED.md](../../internal/fixed-suite-bugs/hibernate-assertionfailederror-longtail-triage-FIXED.md) — archived 2026-07-15 after complete current-binary validation of the scattered assertion, timeout, schema-generation, and one-off catalog.
- [hib-notests-abstract-baseclass-list.md](../../internal/hib-notests-abstract-baseclass-list.md) — 91 classes, NOT a bug (abstract base classes with 0 discoverable tests, matches HotSpot).

## Already tracked elsewhere (not re-documented here)

- `bootstrap.scanning.{JarVisitorTest,ScannerTest,PackagedEntityManagerTest}` jar-scanning `orm.xml doesn't exist` — [hib-proxyclassreuse-loader-blind-class-resolution.md](../hib-proxyclassreuse-loader-blind-class-resolution.md).
- `type.temporal.*` ABORTED entries (`InstantTests`, `LocalDateTimeTest`) and `bytecode.enhancement.basic.{InheritedTest,MappedSuperclassTest}` ABORTED — expected `@CustomEnhancementContext`/dialect-gated partial skips, matches HotSpot per [hib-bytecode-enhancement-loader-faithful-linking.md](../hib-bytecode-enhancement-loader-faithful-linking.md).
