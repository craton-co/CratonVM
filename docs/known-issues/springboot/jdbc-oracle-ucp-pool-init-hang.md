# `spring-boot-jdbc` — Oracle UCP (`PoolDataSourceImpl`) connection-pool tests HANG

**Status: OPEN — found 2026-07-17**

## Symptom

Both Oracle UCP-specific test classes HANG (timed out, zero JUnit output —
`.out.log` is 0 bytes for both):

| Class | Log |
|---|---|
| `org.springframework.boot.jdbc.autoconfigure.OracleUcpDataSourceConfigurationTests` | `shard3/logs/module_spring-boot-jdbc.org.springframework.boot.jdbc.autoconfigure.OracleUcpDataSourceConfigurationTests.err.log` (518 lines) |
| `org.springframework.boot.jdbc.metadata.OracleUcpDataSourcePoolMetadataTests` | `shard3/logs/module_spring-boot-jdbc.org.springframework.boot.jdbc.metadata.OracleUcpDataSourcePoolMetadataTests.err.log` (519 lines) |

Full logs (paths relative to repo root):
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-jdbc.org.springframework.boot.jdbc.autoconfigure.OracleUcpDataSourceConfigurationTests.{out,err}.log`,
`.../module_spring-boot-jdbc.org.springframework.boot.jdbc.metadata.OracleUcpDataSourcePoolMetadataTests.{out,err}.log`

Both `.err.log`s show the same shape: normal post-clinit-fixup startup,
then a long, uninterrupted run of `cratonvm::gc::guard`
`gen_heap::get_field: out-of-bounds field read dropped` warnings (the
benign, already-documented `InterceptingExecutableInvoker`/
`InvocationInterceptorChain` JUnit-timeout-extension noise — see
`docs/known-issues/springboot/modifiedclasspath-aether-network-hang-cluster.md`
for the general pattern) — but **also** repeated bursts on
`org/springframework/core/$Proxy30` and `$Proxy31` (`num_slots=1`,
`real_field_count=Some(1)`), cycling every ~1-2 seconds, right up to the
line cap with no other content (no `SBRUNNER_RESULT`, no stack trace, no
`System.exit`).

This is a **different** signature from the `@ClassPathExclusions`/
`@ClassPathOverrides` HANG cluster also present in this batch (see
`jdbc-classpathexclusions-hang-duplicate-note` below) — neither
`OracleUcpDataSourceConfigurationTests` nor `OracleUcpDataSourcePoolMetadataTests`
source imports `ClassPathExclusions`/`ClassPathOverrides` (confirmed by
reading `OracleUcpDataSourceConfigurationTests.java` in full), and the log
length/content differs (500+ lines cycling on `$Proxy30`/`$Proxy31`
addresses vs. 230 lines cycling on `InterceptingExecutableInvoker` alone).

## Root cause (hypothesis, not confirmed)

`OracleUcpDataSourceConfigurationTests.testDataSourceExists()` builds a
real `oracle.ucp.jdbc.PoolDataSourceImpl` (via
`spring.datasource.type=oracle.ucp.jdbc.PoolDataSource`, wrapping an
embedded H2/HSQLDB driver, not a real Oracle network endpoint — the default
`DataSourceAutoConfiguration` embedded-URL fallback applies) and calls
`.getConnection()`, which triggers genuine Oracle UCP pool
initialization/warm-up. Oracle UCP is a large third-party connection-pool
implementation with its own JMX registration, background maintenance
`Timer`/executor threads, and internal dynamic-proxy-based connection
wrappers (matching the repeating `$Proxy30`/`$Proxy31` addresses in the
log — likely JDK dynamic proxies UCP builds around `java.sql.Connection`/
`DataSource` for its own instrumentation).

The steady ~1-2s-cadence repeat (not a tight spin, not literal silence)
suggests some UCP-internal retry/poll loop (connection validation, pool
maintenance heartbeat, or JMX/config-change polling) that normally
terminates quickly on HotSpot but never converges here — possibly because a
step in that loop depends on a CratonVM behavior difference (a
`Timer`/`ScheduledExecutorService` firing semantics gap, a proxy-dispatch
quirk causing the loop's exit condition to never evaluate true, or a
blocking JDBC/connection-validation call that doesn't error out the way it
would with a genuinely absent driver).

**Not confirmed** — this pass did not attach a debugger or thread-dump the
hung process (out of scope: no test/build execution was performed for this
triage), and the exact UCP internal call chain producing the repeating
`$Proxy30`/`$Proxy31` field reads was not traced. The strongest next step
is a live repro with a thread dump taken mid-hang to identify which UCP
thread/loop is spinning.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-jdbc` | `org.springframework.boot.jdbc.autoconfigure.OracleUcpDataSourceConfigurationTests` |
| `module/spring-boot-jdbc` | `org.springframework.boot.jdbc.metadata.OracleUcpDataSourcePoolMetadataTests` |
