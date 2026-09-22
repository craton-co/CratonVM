# Hibernate ORM suite — class-by-class non-passed census

| | |
|---|---|
| **Measured** | 2026-09-21/22, local Windows checkout, commit `6989206e5` ("perf: retire per-allocation TLAB globals"), CratonVM `C:/craton/CVM/target/release/cratonvm.exe`, real JDK 25, all-default flags (no GC/JIT overrides), `--jdk real` (adds `--java-home` only — no explicit compatible/jdk-only mode flag, so the binary's own default applies, which is `--jdk-only` on this dev tip) |
| **Method** | fork-per-class, 6 shards, `run-hib.sh --category passed --count 0` then `--category others --count 0` (the runner has no single `all` category) — 4548 classes total |
| **Census** | 4432 PASS / 18 FAIL / 2 HANG / 2 CRASH / 3 ABORTED / 91 NOTESTS |
| **Sources** | `apps/hib-suite-runner/runs/run-20260921-232848-passed/on-real/results.tsv` (4453 classes) + `apps/hib-suite-runner/runs/run-20260922-032419-others/on-real/results.tsv` (95 classes) |

This is a fresh correctness sweep at the project's current defaults (post `--jdk-only`-as-default), not a diff against a prior baseline — no earlier full-suite Hibernate ORM class-by-class run at these exact settings was available to compare against in this pass. Treat every row below as "currently non-passing," not necessarily "newly broken."

NOTESTS (91) is a separate, non-failure bucket: `found>0 started=0` or similar vacuous discovery, not scored here as a defect. See the project's own history with this exact shape in the Enumeration$Impl story (`docs/known-issues/jdk-only/...`) — a `NOTESTS` count is a discovery gap, not evidence a class fails, and re-running it under a fixed harness is the right next step before reading it as red.

## The 18 FAIL, by family

### Temporal / timezone handling — 4, likely one shared root cause

| class | found/ok/failed/aborted | ms |
|---|---:|---:|
| `org.hibernate.orm.test.type.temporal.OffsetDateTimeTest` | 488 / 116 / 316 / 56 | 724299 |
| `org.hibernate.orm.test.type.temporal.ZonedDateTimeTest` | 608 / 116 / 435 / 57 | 749540 |
| `org.hibernate.orm.test.type.temporal.OffsetTimeTest` | 396 / 77 / 225 / 37 (57 skipped) | 226986 |
| `org.hibernate.orm.test.type.temporal.LocalDateTimeTest` | — | — (see HANG below, same package) |

These four are the whole of `org.hibernate.orm.test.type.temporal` that didn't cleanly pass, and three of them fail the majority of their own sub-tests (not a handful of edge cases) — `ZonedDateTimeTest` fails 435/608. That shape, plus the shared package, argues for one mechanism under all four rather than four independent bugs. **Not yet root-caused** — the next step is a single-class rerun with `RUST_LOG` or the project's own temporal-native tracing to find which zone/offset conversion path is wrong, then check whether it is jdk-only-specific.

### Multitenancy — 4

| class | found/ok/failed | ms |
|---|---:|---:|
| `org.hibernate.orm.test.multitenancy.DatabaseMultiTenancyTest` | 3/1/2 | 6708 |
| `org.hibernate.orm.test.multitenancy.DatabaseTimeZoneMultiTenancyTest` | 1/0/1 | 6405 |
| `org.hibernate.orm.test.multitenancy.beancontainer.MultiTenantConnectionProviderFromBeanContainerTest` | 4/2/2 | 6428 |
| `org.hibernate.orm.test.multitenancy.beancontainer.MultiTenantConnectionProviderFromSettingsOverBeanContainerTest` | 4/2/2 | 7092 |

Undiagnosed. Worth checking whether these four share the same underlying multitenancy connection-routing mechanism before treating them as separate.

### Stored procedures — 3

| class | found/ok/failed | ms |
|---|---:|---:|
| `org.hibernate.orm.test.sql.storedproc.ResultMappingTest` | 4/0/4 | 88371 |
| `org.hibernate.orm.test.sql.storedproc.StoredProcedureResultSetMappingTest` | 1/0/1 | 23466 |
| `org.hibernate.orm.test.sql.storedproc.StoredProcedureTest` | 4/2/2 | 65154 |

Plus `org.hibernate.orm.test.jpa.procedure.StoredProcedureResultSetMappingTest` (1/0/1, 6353ms) — a fourth, differently-packaged class with the same simple name as one above. Undiagnosed; likely a shared stored-procedure result-mapping defect, not four independent ones.

### JPA boot / persistence discovery — 2

| class | found/ok/failed | ms |
|---|---:|---:|
| `org.hibernate.orm.test.jpa.boot.PersistenceConfigurationTests` | 7/4/3 | 19426 |
| `org.hibernate.orm.test.jpa.boot.discovery.SimpleTests` | 2/1/1 | 14116 |

### Individually undiagnosed — 5

| class | found/ok/failed/aborted/skipped | ms | note |
|---|---:|---|---:|---|
| `org.hibernate.orm.test.delegation.SessionDelegatorBaseImplTest` | 1/0/1/0/0 | 7754 | |
| `org.hibernate.orm.test.hql.HqlParserMemoryUsageTest` | 1/0/1/0/0 | 69484 | name suggests a memory-pressure/perf-sensitive assertion — check host load before treating as a hard fail |
| `org.hibernate.orm.test.schemaupdate.SchemaMigrationTargetScriptCreationTest` | 1/0/1/0/0 | 71295 | |
| `org.hibernate.orm.test.sql.ast.ParameterMarkerStrategyTests` | 5/4/1/0/0 | 123406 | |
| `org.hibernate.orm.test.jpa.lock.LockTest` | 23/14/1/0/8 | 104733 | |

## HANG — 2

| class | rc | timeout | note |
|---|---|---|---|
| `org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest` | 124 | 3600s | this is the class that appeared stuck at 2/17 results for 25+ minutes mid-session with no visible progress; it genuinely ran to the full 3600s cap and was killed — the harness's timeout did fire, just very late. Worth a closer look at why this one class needs an hour before the watchdog can even distinguish it from a live hang. |
| `org.hibernate.orm.test.type.temporal.LocalDateTimeTest` | 124 | 300s | same package as the three temporal FAILs above — very likely the same root mechanism, just manifesting as a hang instead of partial failure for this one class |

## CRASH — 2, both `rc=127`

| class | rc | timeout |
|---|---|---|
| `org.hibernate.orm.test.softdelete.collections.MappingTests` | 127 | 300s |
| `org.hibernate.orm.test.type.temporal.InstantTests` | 127 | 300s |

`rc=127` is unusual for a real crash signal (normally a SIGSEGV shows as `rc=139`, SIGABRT as `134`); it more commonly means "command not found" at the shell level, which would point at a harness/classpath problem rather than a CratonVM defect — but the runner's own convention for this suite is `process-died rc=127 timeout=300s`, i.e. it reports the raw exit code of a process that terminated abnormally within the timeout window (not a timeout kill). **Not yet distinguished from a harness artifact — check the corresponding log files before treating these as VM crashes.** Note `InstantTests` is the third `org.hibernate.orm.test.type.temporal` class to show up non-clean in this run (after the three FAILs and `LocalDateTimeTest`'s HANG) — the temporal package is 5 for 5 non-clean across every failure mode this sweep saw, which is the strongest single signal in this census.

## ABORTED — 3

| class | found/ok/aborted | ms |
|---|---:|---:|
| `org.hibernate.orm.test.bytecode.enhancement.basic.InheritedTest` | 4/3/1 | 6952 |
| `org.hibernate.orm.test.bytecode.enhancement.basic.MappedSuperclassTest` | 4/3/1 | 6004 |
| `org.hibernate.orm.test.manytomanyassociationclass.surrogateid.generated.ManyToManyAssociationClassGeneratedIdTest` | 6/3/3 | 29761 |

## Open items

1. **The `org.hibernate.orm.test.type.temporal` package is the single biggest cluster** — 3 FAIL + 1 HANG + 1 CRASH, i.e. every non-`OK` outcome shape the sweep produced shows up in this one package. Root-causing this one mechanism would likely close 5 of the 18+2+2 rows above.
2. **CRASH rows carry `rc=127`**, not the usual native-crash exit codes — verify against the actual `.log`/stderr before attributing to a VM defect rather than a harness quirk.
3. Multitenancy (4) and stored-procedure result mapping (4, across two packages) are each internally consistent enough to suspect one shared cause per cluster, but neither has been investigated yet.
