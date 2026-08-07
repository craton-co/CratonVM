# `-XX:+UseZGC` full-suite run — 1 crash (fix regression, worse under ZGC), 4 fails (1 new)

**Status:** OPEN (2026-08-06/07). Full 4548-class Hibernate suite,
`-XX:+UseZGC`, 4 shards, real JDK, JIT on, binary `CratonVM-hib-local-0712-v3`
(dev tip `a526ca521`, includes the JIT dynamic-proxy dispatch fix from the
same day). `PASS=4445/4548` (97.8%), zero HANGs. Results:
`apps/hib-suite-runner/runs/categorize-20260806-232032/results.tsv`.

Companion to `g1-collector-fullsuite-crashes-hangs-fails-20260806.md` (same
day, same binary, `-XX:+UseG1GC` instead). ZGC is markedly cleaner than G1 for
this suite (0 HANGs vs 4, 1 CRASH vs 3, sum_ms 52.4M vs 62.9M) but its one
crash is more concerning: it's the **same bug my same-day JIT fix targeted**,
recurring in a *worse* form under ZGC specifically.

## Headline finding: the JIT proxy-dispatch fix does not fully cover ZGC

`DefaultCatalogAndSchemaTest` — fixed to clean `132/132` under the default
collector earlier the same day
(`../../internal/fixed-suite-bugs/hibernate/defaultcatalogandschematest-jit-proxy-dispatch-abstractmethoderror-FIXED-20260806.md`)
— fails under ZGC with the **identical root cause the fix targeted**:

```
java.util.ServiceConfigurationError: org.hibernate.bytecode.spi.BytecodeProvider:
  Provider org.hibernate.bytecode.internal.bytebuddy.BytecodeProviderImpl could not be instantiated
Caused by: java.lang.AbstractMethodError: method net/bytebuddy/utility/Invoker.invoke(...)
  has no Code attribute
	at ... JavaDispatcher$Dispatcher$ForNonStaticMethod.invoke ...
```

**But the failure rate is qualitatively different.** Under the default
collector (pre-fix), this hit 3 of 132 parameterized methods — an
occasional JIT-tiering-timing miss. Under ZGC, **every single method that
touches `produceSessionFactory` fails this way** (9 distinct methods
observed failing consecutively: `createSchema_fromSessionFactory`,
`sequenceGenerator`, `updateSchema_fromSessionFactory`,
`dropSchema_fromSessionFactory`, `entityPersister`, `tableGenerator`,
`enhancedSequenceGenerator`, `enhancedTableGenerator`, and finally
`incrementGenerator` fails a *different* way — see below). This class-wide
failure rate is consistent with the `JavaDispatcher.INVOKER` static proxy
never successfully routing through the fix under ZGC at all, rather than an
occasional cache-miss race — i.e. this looks like a **different code path
reaching the same unguarded resolution point**, not the same rare race
recurring more often. The fix (`vm/src/vm/vm_exec.rs::invoke_or_native`) is
keyed on `invoke_kind` 0/2 per its own verification notes; worth checking
whether ZGC's JIT tiering reaches this call site via a different
`invoke_kind`, or whether the interpreter path itself (which never needed
the fix under the default collector — see the original bug's writeup) is
somehow implicated here.

### Then it gets worse: a cascading fatal exit

After the 9th failing method (`incrementGenerator`), the class first hits an
unrelated-looking `InvalidMappingException` during XML mapping-document
parsing (`org.hibernate.boot.jaxb.internal.InputStreamXmlSource.fromStream`
→ `JAXBContext.newInstance` failing), and then the **entire process** dies
with an uncaught exception from deep inside JUnit Platform's own launcher
internals:

```
[cratonvm] main-vm run() returned Err: Exception in thread "main"
  org/junit/platform/commons/PreconditionViolationException
	at org/junit/platform/commons/util/Preconditions.notBlank(Preconditions.java)
	at org/junit/platform/launcher/core/LauncherConfigurationParameters.getProperty(...)
	at org/junit/platform/launcher/core/LauncherConfigurationParameters.get(...)
	...
```

This is the same *shape* as the (unrelated-mechanism) SmokeTests crash from
2026-08-04/05 — a chain of increasingly implausible failures culminating in
JUnit's own internals throwing on something that should never be blank/null —
the signature of VM-side state corruption cascading outward rather than a
single clean Java-level bug. Whether this specific cascade traces back to the
same `Proxy$Instance`/`Invoker` dispatch gap, or is a second, independent
defect exposed once the class is already in a bad state from the first 9
failures, is not established. The harness records the whole class as
`CRASH` (`process-died rc=1`) because no `@@RESULT` line was ever printed.

**Not yet fixed.** This needs the same kind of investigation as the original
fix — trace `JavaDispatcher.INVOKER`'s dispatch under `-XX:+UseZGC`
specifically, ideally with a `--nojit` control run (does it still fail? that
would refute the JIT-dispatch-gap read entirely) and `CRATONVM_DBG_JIT_NAMES=1`.

### Repro
```
cd apps/hib-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv> --java-home "<jdk25>" --Xmx 1500m \
  -XX:+UseZGC @common.args -Dcraton.batch=1 CratonRunner \
  org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest
```
Single occurrence, one run — not yet confirmed deterministic, though the
9/9-consecutive-methods shape within that one run is itself suggestive of a
hard failure rather than a timing-sensitive flake.

## New: `ZonedDateTimeTest` — synthetic stand-in leaking into a `long[]` cast

5 real failures (not the usual dialect-assumption `ABORTED` self-skips —
those still occur normally alongside these) across 4 distinct test methods
(`testRetrievingEntityByZonedDateTime` ×2 parameterizations,
`writeThenRead`, `writeThenNativeRead`, `nativeWriteThenRead`), all with the
same root cause:

```
java.lang.RuntimeException: Could not build SessionFactory: Unable to set
  JDBC Connection auto-commit mode in preparation for DDL execution
  [General error: "java.lang.ClassCastException:
  cratonvm.synthetic.AnonymousObject$3 cannot be cast to [J"; SQL statement:
  COMMIT [50000-240]] [n/a]
```

`cratonvm.synthetic.AnonymousObject$3` is one of CratonVM's own synthetic
stand-in classes (used where a real JDK/library class isn't fully modeled)
leaking into H2's own internal commit-handling code where it expects a real
`long[]` (`[J`). This is genuinely new — not seen in the default-collector or
G1 runs the same day. Not yet root-caused: worth checking what
`AnonymousObject$3` stands in for in this call path (H2's `commit()`
internals during DDL auto-commit setup) and whether ZGC's different
allocation/collection timing is exposing a pre-existing synthetic-class
identity bug or whether it's ZGC-specific machinery itself producing the bad
cast.

### Repro
```
cd apps/hib-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv> --java-home "<jdk25>" --Xmx 1500m \
  -XX:+UseZGC @common.args -Dcraton.batch=1 CratonRunner \
  org.hibernate.orm.test.type.temporal.ZonedDateTimeTest
```
Single run, not yet confirmed deterministic.

## Not new — already-documented residuals recurring unchanged

- `org.hibernate.orm.test.batch.BatchTest` — `testBatchInsertUpdate` 120s
  `TimeoutException`, the known generic throughput-margin residual
  (`hib-120s-junit-timeout-cluster-20260716.md`'s recurrence notes). Not
  ZGC-specific.
- `org.hibernate.orm.test.jpa.lock.LockTest` — `found=23 ok=14 failed=1
  skipped=8`, exact match to the confirmed non-bug
  (`locktest-pessimistic-write-timeout-is-not-a-vm-bug-20260730.md`): real
  HotSpot misses the same hardcoded 5000ms budget under host load. Not
  actionable, not ZGC-specific.
- `org.hibernate.orm.test.sql.exec.SmokeTests` — `found=17 ok=16 failed=1`,
  exact match to the well-documented `testQueryConcurrency` throughput
  timeout (`../../internal/fixed-suite-bugs/hibernate/smoketests-concurrent-query-throughput-20260723-RETIRED.md`).
  Not ZGC-specific.

## Timing

Sum of per-class ms across all 4548 classes: **52,442,317 ms (874.0 min)**,
4 shards, 267m40s wall. Lighter than the same-day G1 run's 62,897,431 ms
(1048.3 min) — ZGC is the cheaper of the two non-default collectors for this
suite, by the shard-count-independent per-class-sum metric.

## Related

- `g1-collector-fullsuite-crashes-hangs-fails-20260806.md` — the G1 sibling
  run, same day, same binary. `DefaultCatalogAndSchemaTest` crashes under
  BOTH non-default collectors, but via genuinely different mechanisms (G1:
  native SIGSEGV with a shared fault address across 3 classes; ZGC: a clean
  Java-level `AbstractMethodError` cascading into a fatal JUnit-internals
  exception). Don't conflate the two — they may share a root cause upstream
  (both are non-default-collector-specific) or may be unrelated; not
  established either way.
- `../../internal/fixed-suite-bugs/hibernate/defaultcatalogandschematest-jit-proxy-dispatch-abstractmethoderror-FIXED-20260806.md`
  — the fix this doc's headline finding shows is incomplete under ZGC.
