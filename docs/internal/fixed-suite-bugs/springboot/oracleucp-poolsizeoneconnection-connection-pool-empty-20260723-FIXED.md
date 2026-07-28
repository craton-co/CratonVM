# `OracleUcpDataSourcePoolMetadataTests.getPoolSizeOneConnection` — first on-demand UCP connection borrow reports "pool is empty"

**Status: OPEN — found 2026-07-23**

## Closure — verified 2026-07-28

The reported UCP-29 first-borrow failure was already resolved by the JDBC
closure merged as `2f8249c36` on 2026-07-24. The original report remained in
`docs/known-issues` after that delivery, so it was stale rather than an
active residual.

On a fresh release build from the initial `dev` snapshot (`b695d468f`), using Eclipse
Temurin 25.0.3.9 and the real `apps/spring-boot` fixture, the full six-test
`OracleUcpDataSourcePoolMetadataTests` class passed on every fresh VM process:

| Mode | Fresh class processes | Tests | Failures / aborts / failed containers |
|---|---:|---:|---:|
| JIT | 3 | 18 | 0 / 0 / 0 |
| `--nojit` | 3 | 18 | 0 / 0 / 0 |

The JIT runs completed in 7.8 s, 6.9 s, and 6.6 s; the interpreter runs
completed in 6.8 s, 6.6 s, and 5.7 s. This directly covers the historically
timing-sensitive first on-demand borrow in `getPoolSizeOneConnection` in both
execution modes.

After merging current `origin/dev` into the delivery branch, a second fresh
fat-LTO release build also passed the full class with JIT (6/6, 5.8 s) and
`--nojit` (6/6, 6.5 s), so the retirement is validated on the final integrated
runtime as well.

The broader 52-class `spring-boot-jdbc` run could not be repeated on this
host because the supplied fixture has an incomplete, untracked Gradle tree:
its cached JDBC classpath references a missing
`spring-boot-autoconfigure-4.1.0-SNAPSHOT.jar`, and Gradle cannot regenerate
it because `build-plugin/spring-boot-antlib` is absent. Those resulting
missing-class failures are fixture invalidation, not VM residuals. The
previous valid full-module closure remains 51 PASS plus one expected EMPTY
class in both modes; the current direct evidence above is the valid regression
gate for this retired issue.

## Original symptom

`module/spring-boot-jdbc`'s `OracleUcpDataSourcePoolMetadataTests` passes
5/6 tests; `getPoolSizeOneConnection()` fails on the very first JDBC
connection acquisition against a freshly-constructed, empty
(`minPoolSize=0`, `maxPoolSize=2`) Oracle UCP `PoolDataSource`:

```
=> org.springframework.jdbc.CannotGetJdbcConnectionException: Failed to obtain JDBC Connection
     org.springframework.jdbc.datasource.DataSourceUtils.getConnection(DataSourceUtils.java:84)
     org.springframework.jdbc.core.JdbcTemplate.execute(JdbcTemplate.java:362)
     org.springframework.boot.jdbc.metadata.AbstractDataSourcePoolMetadataTests.getPoolSizeOneConnection(AbstractDataSourcePoolMetadataTests.java:65)
   Caused by: java.sql.SQLException: UCP-29: Failed to get a connection
     oracle.ucp.jdbc.PoolDataSourceImpl.getConnection(PoolDataSourceImpl.java:2209)
   Caused by: oracle.ucp.UniversalConnectionPoolException: UCP-45069: Universal Connection Pool is empty - [ 0, 0, 0, 0, 0, 0, 1, 2, 0, 0 ]
     oracle.ucp.common.UniversalConnectionPoolImpl.borrowConnectionWithoutCountingRequests(UniversalConnectionPoolImpl.java:975)
     oracle.ucp.common.UniversalConnectionPoolImpl.borrowConnectionAndValidate(UniversalConnectionPoolImpl.java:527)
     oracle.ucp.jdbc.JDBCConnectionPool.borrowConnection(JDBCConnectionPool.java:249)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard2/logs/module_spring-boot-jdbc.org.springframework.boot.jdbc.metadata.OracleUcpDataSourcePoolMetadataTests.out.log`

## Not the previously-fixed UCP hang

This class/method is **not** a recurrence of
[`../../internal/fixed-suite-bugs/springboot/jdbc-oracle-ucp-pool-init-hang-FIXED.md`](../../internal/fixed-suite-bugs/springboot/jdbc-oracle-ucp-pool-init-hang-FIXED.md)
(a synthetic-`Semaphore`/real-AQS layout mismatch that made
`Semaphore$Sync.reducePermits()` spin forever, fixed 2026-07-18). That fix's
own validation table shows this exact class passing 6/6 afterward. The
current failure is qualitatively different: it's not a hang (whole run
finishes in 10.3s) and not a spin — it's an immediate, clean exception
reporting the pool has zero connections and cannot grow, on the *first ever*
borrow from a fresh `PoolDataSourceImpl`.

## Root cause — hypothesis, not confirmed

Each test method in `AbstractDataSourcePoolMetadataTests`'s subclasses gets
its own fresh `PoolDataSource` (`OracleUcpDataSourcePoolMetadataTests`'s
`dataSourceMetadata` field is a per-instance JUnit5 field, and JUnit5 builds
a new test instance per method by default), so this isn't cross-test pool
contention. `getPoolSizeNoConnection`, `getIdle`, and
`getPoolSizeTwoConnections` all perform the same
`jdbcTemplate.execute(ConnectionCallback)` pattern against their own equally
fresh, equally-empty (`minPoolSize=0`) pool and pass — so the defect is not
"UCP can never grow the pool from 0", only something specific to this one
call shape/timing.

With `minPoolSize=0`, UCP must synchronously create a brand-new physical
connection on the very first borrow rather than handing out an already-idle
one (there are none). The UCP-45069 message's state array
(`[0, 0, 0, 0, 0, 0, 1, 2, 0, 0]`) shows a pool that still has 0 available
and 0 borrowed at the moment of the exception, with what look like the
configured min(1?)/max(2) values further along — consistent with the borrow
call observing the pool *before* on-demand growth has actually produced a
connection, then giving up rather than waiting/retrying for growth to
complete. This points at a **timing/wait-notification race** between UCP's
connection-borrow wait logic (`UniversalConnectionPoolImpl.borrowConnectionAndValidate`,
third-party `ucp.jar`, not CratonVM source) and whatever CratonVM
concurrency primitive it blocks on internally (a `wait()`/`Condition`/`park()`
under the pool's lock) — the same general class of gap as other
documented CratonVM lock/monitor-semantics differences (see
`reference_per_thread_native_caches_need_all_six_root_paths`,
`reference_cpu_sample_deadlock_vs_slow_technique` in project memory), but
**not pinned to a specific CratonVM file or function in this session** —
`oracle.ucp.*` is a third-party jar (no CratonVM source to read), and no
debugger/thread-dump was attached to catch the pool mid-borrow.

**What would confirm/refute this:** attach a debugger or add
`-Doracle.ucp.debug` logging to a standalone repro (construct a
`PoolDataSourceImpl` with `minPoolSize=0`/`maxPoolSize=2`, immediately borrow
once) and compare the exact sequence of internal UCP thread creation and
lock/wait calls between CratonVM and real HotSpot; if CratonVM's borrow path
returns from a wait/park call early (spurious wakeup not correctly
re-checked) or a background pool-growth thread's completion signal isn't
observed by the borrowing thread, that would confirm a monitor-semantics
gap rather than anything UCP-specific.

## Affected classes

| Module | Class | Method |
|---|---|---|
| `module/spring-boot-jdbc` | `org.springframework.boot.jdbc.metadata.OracleUcpDataSourcePoolMetadataTests` | `getPoolSizeOneConnection` (1 of 6 tests) |
