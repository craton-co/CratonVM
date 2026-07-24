# `spring-boot-jdbc` residual closure — FIXED

**Fixed: 2026-07-24**

## Scope and causes

This closure covered the Spring Boot `module/spring-boot-jdbc` residual set,
including `HikariDataSourceConfigurationTests`,
`DataSourceAutoConfigurationWithoutSpringJdbcTests`,
`DataSourceUnwrapperNoSpringJdbcTests`,
`EmbeddedDatabaseConnectionTests`, and
`OracleUcpDataSourcePoolMetadataTests`, plus full-module residual discovery.

Four independent VM defects were fixed:

1. Bootstrap-appended Mockito classes were incorrectly treated as local classes
   by isolated URL class loaders, and nested annotation classes could resolve to
   a same-named class from a different defining loader. Loader-aware exact
   resolution now preserves defining-loader identity.
2. UCP's zero-wait asynchronous pool growth could surface UCP-29 before its
   first connection worker completed under the interpreter. CratonVM now uses
   UCP's documented borrow-thread creation policy by default; explicit
   application configuration remains authoritative.
3. `FormatableHashtable` inherited from `Hashtable` but was initialized and
   sized as a `HashMap`. The mismatched `size` field made Derby serialize a
   truncated hashtable stream. Native collection storage now honors Hashtable's
   `count`, `threshold`, and bucket layout while leaving `Properties` on its
   separate JDK 25 path.
4. The shared charset alias resolver omitted JDK's historic `8859_1` spelling.
   c3p0 uses that alias when reading its resource-path file, so
   `DataSourceAutoConfigurationTests` failed with
   `UnsupportedEncodingException: 8859_1`. It now resolves to `ISO-8859-1`.

## Validation

JDK: Eclipse Temurin 25.0.3.9. The suite runner used one VM process per class,
two concurrent workers, and a 300-second per-class timeout.

| Mode | Module result |
|---|---|
| `--nojit` | 51 PASS, 1 expected EMPTY (`AbstractDataSourcePoolMetadataTests`) |
| JIT | 51 PASS, 1 expected EMPTY (`AbstractDataSourcePoolMetadataTests`) |

The no-JIT full-module stability run is
`jdbc-module-all-r3-clean-nojit-20260724`; the JIT counterpart is
`jdbc-module-all-r1-clean-jit-20260724`.

Additional targeted checks passed in both modes for the residual list, and the
historically timing-sensitive parallel pair
`EmbeddedDatabaseConnectionTests` plus
`HikariCheckpointRestoreLifecycleTests` passed together with two workers.
