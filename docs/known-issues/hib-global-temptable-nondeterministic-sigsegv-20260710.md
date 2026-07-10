# Hibernate suite — non-deterministic SIGSEGV/HANG cluster, likely a global-temp-table race (2026-07-10)

| | |
|---|---|
| **Status** | 🔴 OPEN — confirmed real, not yet root-caused. Non-deterministic: same class/command produces PASS, HANG, or CRASH across separate runs. |
| **Severity** | High — 20 of 33 classes in a targeted rerun crashed (`rc=139`/SIGSEGV), all with `ms=0` (crashed before printing `@@BEGIN`, or immediately after). |
| **Build** | `dev` at the point `test/hib-remote-rerun-20260708` merged latest `dev` (post the large 2026-07-09/10 merge wave — includes the independently-landed `threadgroup-native-field-index-mismatch-FIXED` fix). |

## Discovery context

While rerunning the (now much-shrunk) Hibernate non-passed list on a remote
Azure host, a 4-shard run produced `CRASH=20` out of 33 classes, all
`process-died rc=139` (SIGSEGV) with `ms=0`. Re-running the identical
4-shard job a second time produced the **exact same 20 classes** crashing —
initially suggestive of a deterministic bug rather than host contention.

## What's ruled out

- **Not simple resource contention**: running one of the crashing classes
  (`DefaultCatalogAndSchemaTest`) completely alone (no concurrent shards,
  freshly-checked host with 28GiB free memory) still reproduces a crash when
  run as the first class of its full 8-class shard batch.
- **But also not a clean deterministic repro**: running the SAME single
  class alone, by itself, in isolation, across 4 separate attempts produced
  4 DIFFERENT outcomes: PASS, PASS, HANG (`rc=124`), and (in the 8-class
  batch context) CRASH (`rc=139`). This rules out both "always crashes" and
  "purely host-load-dependent" — it's genuinely flaky at the CratonVM level,
  most consistent with an internal race condition (e.g. a data race on
  shared/global state, or a heap-corruption bug whose symptom depends on
  memory-layout timing).

## Symptom detail

`DefaultCatalogAndSchemaTest` (and likely the other 19 crashing classes,
not individually confirmed) repeatedly creates and drops **global temporary
ID tables** across its several `@Test` methods — both catalog/schema-
unqualified (`HT_EntityWithJoinedInheritanceWithDefaultQualifiers`) and
explicitly-qualified (`someExplicitCatalog.someExplicitSchema.HT_Entity...`)
variants, via Hibernate's `GlobalTemporaryTableStrategy`. In the batch/crash
run, the raw log shows a clean *first* round of create+drop for all ~13
tables, then a *second* round beginning (same test class, next `@Test`
method's `@BeforeEach` setup) that gets partway through re-creating the
qualified-table variants before the process dies with no further output.

This pattern (works once, breaks on a repeat within the same test-class
lifecycle) is consistent with state that should be reset/cleared between
test methods but isn't always — a stale reference, double-free, or a
GC-root/pin-tracking bug tied to whatever native code backs global-temp-table
DDL execution or catalog/schema-qualified identifier handling.

## Affected classes (from the 2026-07-10 remote rerun, `CRASH` status)

```
boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest
bytecode.enhancement.graph.LoadAndFetchGraphAssociationNotExplicitlySpecifiedTest
joinedsubclassbatch.IdentityJoinedSubclassBatchingTest
query.hql.FunctionTests
type.temporal.OffsetTimeTest
bytecode.enhancement.lazy.LazyOneToOneRemoveFlushAccessTest
hql.ASTParserLoadingTest
type.temporal.InstantTests
type.temporal.ZonedDateTimeTest
batch.BatchTest
bytecode.enhancement.lazy.proxy.FetchGraphTest
id.enhanced.OptimizerConcurrencyUnitTest
manytomanyassociationclass.surrogateid.generated.ManyToManyAssociationClassGeneratedIdTest
onetoone.embeddedid.OneToOneEmbeddedIdTest
sql.exec.SmokeTests
type.temporal.LocalDateTimeTest
batchfetch.DynamicBatchFetchTest
mapping.embeddable.JsonWithArrayEmbeddableTest
onetoone.embeddedid.OneToOneJoinColumnsEmbeddedIdTest
type.temporal.OffsetDateTimeTest
```

Not all of these necessarily share the exact same root cause — only
`DefaultCatalogAndSchemaTest` was individually probed. Many share the
"multiple `@Test` methods, each doing substantial DDL/temp-table setup"
shape (batch inserts, joined/table-per-class inheritance, temporal
round-trips), which is circumstantial support for a shared temp-table/DDL
lifecycle bug, not proof.

## Repro

Azure host, harness at `/data/data/hibpkg/runner` (or equivalent):
```bash
echo org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest > /tmp/one.txt
# Run several times in a row — expect inconsistent PASS/HANG/CRASH outcomes:
for i in 1 2 3 4; do
  timeout 120 <cratonvm> --java-home <jdk25> --Xmx 1500m @common.linux.args \
    -Dcraton.batch=1 CratonRunner /tmp/one.txt 0
  echo "rc=$?"
done
```

## Next steps (not yet done)

- Get a core dump / gdb backtrace from an actual SIGSEGV occurrence (the
  `timeout` wrapper reports "the monitored command dumped core" but no core
  file location was captured in this pass — enable core dumps
  (`ulimit -c unlimited`) and point `core_pattern` somewhere writable first).
- Since this is flaky rather than deterministic, a single repro won't do —
  script a loop (10-20x) capturing full stderr + a core dump on first
  failure, then `gdb <binary> <core>` for a backtrace.
- Check whether this reproduces with `--nojit` (rules out JIT-specific
  timing/codegen involvement in the race).
- Given the "global temp table repeated create/drop across @Test methods"
  pattern, look first at whatever native code path backs
  `GlobalTemporaryTableStrategy`'s DDL execution and any catalog/schema-
  qualified-identifier caching/interning — especially anything involving
  shared mutable state without proper synchronization or GC-root pinning
  across repeated calls.
