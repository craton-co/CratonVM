# Hibernate FAIL-bucket triage on Linux (Azure host, dev `0d142fad`+, 2026-07-03)

Source: the 387-class non-passed set from a full-suite Linux run (real-JDK,
JIT on, TIMEOUT=600), OSR-off baseline (239 FAIL). After excluding the
already-tracked clusters (bytecode.enhancement/lazytoone ~187, jar-scanning 3,
temporal-GC 8, cascade-null 2), **39 classes remained unclassified**. Triaged
below into known-but-broader, environment/packaging (ruled out), and
genuinely new bugs.

## Genuinely new — worth a fix

### A. `NoClassDefFoundError: com/microsoft/sqlserver/jdbc/SQLServerDriver` (3 classes)
`connection.DriverManagerRegistrationTest`, `dialect.resolver.DialectFactoryTest`,
`dialect.resolver.DialectSpecificConfigTest`.

**Verified NOT a packaging gap**: the `mssql-jdbc-13.4.0.jre11.jar` is present
on the classpath, byte-identical to the source (`1547706` bytes both sides),
and its zip contents include `com/microsoft/sqlserver/jdbc/SQLServerDriver.class`
(confirmed via `zipfile` inspection). No `ExceptionInInitializerError` or
"Missing native method" appears immediately before the failure in stderr — the
nearby log lines show `java.sql.DriverManager`'s `ServiceLoader`-based driver
auto-registration succeeding for a DIFFERENT JDBC driver on the same classpath
(`com.huawei.gaussdb.jdbc.Driver`), then failing to resolve the SQL Server one.
**Hypothesis**: a `ServiceLoader`/`META-INF/services/java.sql.Driver`
multi-provider jar-scanning gap — CratonVM's classpath/jar scanning may not
correctly enumerate service providers across multiple JAR files when several
are present (ties to the same general area as the tracked jar-scanning
cluster, though a different failure mode — that one crashes `JarVisitorTest`
directly, this one silently fails to resolve one specific provider).
**Action**: get the full VEH/stderr around `DriverManager.<clinit>` or
`ServiceLoader.load(Driver.class)` to see whether SQLServerDriver's provider
entry is being skipped during the scan, or found-but-then-failing to
`Class.forName`/load.

### B. HIB-CV-27 (in-process javac / H2 stored procs) appears to have REGRESSED on Linux
`delegation.SessionDelegatorBaseImplTest`:
```
SQLGrammarException: Error executing work [Syntax error ... 
  /org/h2/dynamic/FINDONEUSER.java:3: error: package org.h2.tools does not exist
  import org.h2.tools.SimpleResultSet;
```
This is the exact HIB-CV-27 symptom (H2's dynamically-compiled stored
procedures can't see `org.h2.tools.SimpleResultSet`), previously verified
FIXED (see `docs/internal/hibernate-bugs/hib*` for the original fix — need to
locate the exact doc/commit). **`h2-2.4.240.jar` is present and correct on
this host's classpath** (verified). Hypothesis: CratonVM's **in-process javac
classpath construction may hardcode `;` as the path separator** when
reconstructing the compiler's classpath internally, which is a no-op on
Windows but breaks javac's `-cp` parsing on Linux (needs `:`). Grep
`native-builtins/src/phases_late.rs` and `service_loader.rs` around the
"in-process javac" / "HIB-CV-27" comments for where the classpath string is
assembled — check for a literal `";"` join that isn't behind a
`cfg(windows)`/`std::path::MAIN_SEPARATOR`-style platform switch. Related
symptoms in the same 3-class cluster: `sql.storedproc.ResultMappingTest`,
`sql.storedproc.StoredProcedureResultSetMappingTest`,
`sql.storedproc.StoredProcedureTest`, `jpa.procedure.StoredProcedureResultSetMappingTest`
all show `SQLGrammarException: Could not prepare statement` — same in-process
compile/prepare path, same suspected root cause.

### C. Genuine new correctness/serialization bugs (one-offs, not yet root-caused)
- `mapping.type.java.JdbcTimestampJavaTypeTest` — `AssertionFailedError: expected <true> but was <false>` (needs the specific assertion to know what boolean predicate is wrong).
- `mapping.mutability.attribute.BasicAttributeMutabilityTests` — bare `AssertionFailedError`.
- `serialization.CacheKeyEmbeddedIdEnanchedTest` — `InvalidClassException: local class incompatible: stream classdesc serialVersionUID mismatch` — a real serialization/serialVersionUID computation bug (CratonVM likely computes a different implicit serialVersionUID than HotSpot for this embeddable-id cache-key class).
- `softdelete.SoftDeleteFetchModeTests` — `Expecting UnsupportedMappingException...` (expected exception not thrown — a validation gap).
- `mapping.fetch.depth.NoDepthTests` — `PersistenceException: No Persistence provider for ...` — looks like a JPA bootstrap/provider-discovery gap, possibly `META-INF/services/jakarta.persistence.spi.PersistenceProvider` ServiceLoader (same family as finding A?).
- `boot.database.metadata.MetadataAccessTests` — `ServiceException: Unable to create requested service [...]`.
- `engine.spi.EntityEntryTest` — `MockitoException` (Mockito/CratonVM interop gap, distinct from the earlier-tracked Mockito issue in `UUidV6V7GeneratorTest`, which is a known timeout not a Mockito error).
- `timezones.JDBCTimeZoneZonedTest`, `timezones.PassThruZonedTest`, `timezones.UTCNormalizedInstantTest` — all three fail with `expected: <2026-07-03T19:XX:XX...> but was: <...>` (wall-clock timestamp mismatches). Worth checking whether these are flaky (test compares against `Instant.now()`-ish values with a tolerance CratonVM's timing exceeds) vs a genuine timezone-storage correctness bug — distinct from the already-tracked `type.temporal.*` GC-crash cluster (these are FAIL/assertion, not CRASH).

## Ruled out / already tracked (no new action)
- **7 classes shuffling between two already-broken statuses** (FAIL↔HANG↔CRASH in OSR on/off) — see `jit-osr-linux-regression-triad.md`.
- **`proxy.ProxyClassReuseTest`** (`ClassCastException`) — already tracked in `docs/known-issues/hib-proxyclassreuse-loader-blind-class-resolution.md`.
- **`service.ClassLoaderServiceImplTest`** — already tracked (HIB-CV-24 area).
- **9 "Could not build SessionFactory: To-one map..." classes** (`EnhancedProxyCacheTest`, `AutoFlushBeforeLoadTest`, `JoinFetchWithEnhancementTest`, `PrivateConstructorEnhancerTest`, `LockExistingBytecodeProxyTest`, `OneToOneEmbeddedIdTest`, `OneToOneJoinColumnsEmbeddedIdTest`, `DetachedEntityParameterAutoFlushVersionTest`, `BaseIdEntityByteCodeTest`) — enhancement-bootstrap-adjacent, same root cause as the tracked bytecode-enhancement cluster.
- **`query.CachedQueryShallowWithDiscriminatorBytecodeEnhancedTest`** — name says bytecode-enhanced, same cluster.
- **Timeout-classified (known slowness, not new)**: `batch.BatchTest`, `batchfetch.DynamicBatchFetchTest`, `hql.HqlParserMemoryUsageTest`, `id.enhanced.OptimizerConcurrencyUnitTest`, `id.uuid.rfc9562.UUidV6V7GeneratorTest`, `jpa.lock.LockTest`, `sql.exec.SmokeTests` — all part of the known throughput-wall slowness cluster identified earlier this session (need >600s or hit JUnit's internal 120s/method limit, not correctness bugs).
