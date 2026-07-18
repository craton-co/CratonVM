# Spring Boot JDBC / Hikari MBean registration cluster — fixed

**Resolved: 2026-07-18**

## Scope

`spring-boot-jdbc` previously reported absent Hikari pool/config MBeans in
`DataSourceJmxConfigurationTests` and absent lazy `DataSource` MBeans in
`LazyConnectionDataSourceConfigurationTests`. Investigation also covered the
adjacent Hikari checkpoint lifecycle path reached by the same fixture.

## Root causes and fixes

1. HikariConfig copies its private `credentials` field through reflection from
   its declaring class. CratonVM treated every non-public field as inaccessible
   unless `setAccessible(true)` had been used. Field access now permits a
   non-public member when the immediate caller is its declaring class, matching
   Java access rules while retaining the denial for other callers.
2. Hikari's checkpoint/suspend path invokes
   `Semaphore.acquireUninterruptibly(int)`. The existing native semaphore
   bridge covered only the zero-argument overload, causing the fallback AQS
   `UnsupportedOperationException`. The counted overload now uses the real
   semaphore acquisition bridge.
3. The synthetic MBean server stored ObjectNames by their input text. JMX
   identity is canonical: property order is not significant. Spring registers
   `name=dataSource,type=HikariDataSource`, while the test queries the equivalent
   `type=HikariDataSource,name=dataSource`. ObjectName canonicalization now
   drives MBean-server keys, `equals`, `hashCode`, and canonical-name access.

## Regression coverage

Focused native tests:

- same-declaring-class private reflective field access is allowed and a
  different caller remains denied;
- `acquireUninterruptibly(int)` consumes the requested permit count;
- ObjectName canonicalization equates reordered properties and preserves quoted
  commas/property-list patterns.

Release validation used the isolated binary
`cratonvm-hikari-mbeans-final-20260718-019f742a.exe` with the exact
`spring-boot-jdbc` test classpath:

| Mode | Class | Result |
|---|---|---|
| JIT | `DataSourceJmxConfigurationTests` | 8/8 passed |
| JIT | `LazyConnectionDataSourceConfigurationTests` | 6/6 passed |
| JIT | `HikariCheckpointRestoreLifecycleTests` | 6/6 passed |
| `--nojit` | `DataSourceJmxConfigurationTests` | 8/8 passed |
| `--nojit` | `LazyConnectionDataSourceConfigurationTests` | 6/6 passed |
| `--nojit` | `HikariCheckpointRestoreLifecycleTests` | 6/6 passed |

The known-issue record was moved here after this full JIT/interpreter matrix
passed.
