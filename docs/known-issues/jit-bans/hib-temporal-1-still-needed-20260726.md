# HIB-TEMPORAL.1 (`org/hibernate/`, temporal/DDL residual) — CONFIRMED still needed, MORE severe than originally documented

**Status: ban kept, re-verified live with a real Hibernate ORM 8.0 test harness — the actual corruption is a full bootstrap failure, not just a narrow DDL type-descriptor NPE as originally reported.**

## Fixture used

Real Hibernate ORM 8.0 (commit `171b6cb0d`) test harness rebuilt on this
host at `/data/data/apps/hibernate-orm-harness/` (`hib-libs/` = compiled
`hibernate-core` test classes + full `testRuntimeClasspath` jar set,
`hib-suite-runner/CratonRunner.java` = a resumable per-class JUnit5
Platform Launcher driver). Real H2 in-memory database, real
`SessionFactory` bootstrap, real entity mapping.

## Result

| Config | `InstantTests` (204 methods) | `ASTParserLoadingTest` (106) | `HQLInsertAndUpdateTest`/`WithClauseTest`/`EnumTest` (17) |
|---|---|---|---|
| Baseline (`org/hibernate/` banned) | 112 ok / 0 failed / 92 aborted | 106/106 ok | 17/17 ok |
| `CRATONVM_JIT_ALLOW_PACKAGES=org/hibernate/` (lifted) | **32 ok / 148 failed** / 24 aborted | **0 started — setup failure** | 17/17 ok (unaffected) |

Every failure/setup-error in the lifted configuration is the identical
exception:

```
org.hibernate.boot.registry.selector.spi.StrategySelectionException: Default resolver threw exception
```

`StrategySelector` is core Hibernate bootstrap infrastructure (resolves
pluggable strategy implementations — dialects, resolvers, etc. — by
short name or class). A `StrategySelectionException` at this stage means
Hibernate's own bootstrap/service-registry machinery is corrupted when
JIT-compiled, not just the narrower `DdlTypeImpl.getRawTypeName` /
`TIMESTAMP_UTC` descriptor-registration NPE the original 2026-07-08 report
described. This is a **broader and more severe** manifestation of
JIT-compiling `org/hibernate/` than what motivated the original ban.

## Isolation from HIB-ANTLR.1 (`org/antlr/v4/runtime/`)

The above lifted run also had `org/antlr/v4/runtime/` allowed
simultaneously (testing both HIB-TEMPORAL.1 and HIB-ANTLR.1 in one pass).
Re-ran with **only** `org/antlr/v4/runtime/` allowed (`org/hibernate/`
still banned): all three test classes passed clean, 0 failures, matching
baseline exactly. This isolates the `StrategySelectionException`
corruption to `org/hibernate/` alone — see
`docs/internal/jit-bans/hib-antlr-1-removed-shadowed-20260726.md` for that separate
finding, which was closed on 2026-07-27: `org/antlr/v4/runtime/` is no
longer banned as a package at all (HIB-LONGTAIL.1's second prefix was
dropped after a 57-class HQL A/B). HIB-TEMPORAL.1 is unaffected by that
— it is a different package and still needed.

## Disposition

**KEEP `org/hibernate/` banned (`hibernate_temporal_residual_skip_prefix`,
HIB-TEMPORAL.1).** Confirmed live, confirmed severe (a full bootstrap
failure affecting the majority of methods in two real test classes, not
an edge case), confirmed isolated to this exact package (not an
antlr-runtime interaction). No fix attempted — root-causing the
`StrategySelector` corruption under JIT is a substantial follow-up in its
own right, flagged for a dedicated session.

## Reproduction

```bash
cd /data/data/apps/hibernate-orm-harness/hib-suite-runner
echo 'org.hibernate.orm.test.type.temporal.InstantTests' > /tmp/list.txt
echo 'org.hibernate.orm.test.hql.ASTParserLoadingTest' >> /tmp/list.txt
TMPDIR=/data/tmp <cratonvm-binary> --java-home /home/victor/jdk25 @common.args \
  CratonRunner /tmp/list.txt 0
# baseline: clean. Add CRATONVM_JIT_ALLOW_PACKAGES=org/hibernate/ to reproduce
# the StrategySelectionException cascade.
```
