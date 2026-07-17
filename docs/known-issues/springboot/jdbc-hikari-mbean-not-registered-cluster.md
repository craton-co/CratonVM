# `spring-boot-jdbc` — HikariCP pool/pool-config MBeans never actually get registered with the platform `MBeanServer`

**Status: OPEN — found 2026-07-17**

## Symptom

6 test methods across 2 classes assert that, after a `HikariDataSource`
with `register-mbeans=true` is initialized (a connection obtained and
closed to force pool startup), `MBeanServer.isRegistered(new
ObjectName("com.zaxxer.hikari:type=Pool (" + poolName + ")"))` (and the
`PoolConfig` sibling) is `true`. On CratonVM it is `false` — the MBeans were
never registered at all (this is a different symptom than the
already-`FIXED` `BUG-TC0622` — that bug was about `queryNames`/`queryMBeans`
enumeration returning empty for beans that genuinely *were* registered;
here direct `isRegistered(ObjectName)` on a *specific* name returns `false`,
i.e. registration itself did not happen).

| Class | Failing tests |
|---|---|
| `org.springframework.boot.jdbc.autoconfigure.DataSourceJmxConfigurationTests` | `hikariAutoConfiguredCanUseRegisterMBeans`, `hikariAutoConfiguredUsesJmxFlag`, `hikariProxiedCanUseRegisterMBeans` (all fail at `validateHikariMBeansRegistration`, `DataSourceJmxConfigurationTests.java:137-140`); `hikariAutoConfiguredWithoutDataSourceName` (queries `com.zaxxer.hikari:type=*` and expects `existingInstances.size() + 2`, got `existingInstances.size() + 0`) |
| `org.springframework.boot.jdbc.autoconfigure.LazyConnectionDataSourceConfigurationTests` | `autoConfigurationExposeDataSourceMBeanWhenEnabled` (both `[eager]`/`[lazy]` parameterizations) |

Representative trace:

```
org.opentest4j.AssertionFailedError:
expected: true
 but was: false
       org.springframework.boot.jdbc.autoconfigure.DataSourceJmxConfigurationTests.validateHikariMBeansRegistration(DataSourceJmxConfigurationTests.java:138)
       org.springframework.boot.jdbc.autoconfigure.DataSourceJmxConfigurationTests.lambda$hikariAutoConfiguredUsesJmxFlag$0(DataSourceJmxConfigurationTests.java:112)
```

```java
private void validateHikariMBeansRegistration(MBeanServer mBeanServer, String poolName, boolean expected) {
	assertThat(mBeanServer.isRegistered(new ObjectName("com.zaxxer.hikari:type=Pool (" + poolName + ")")))
		.isEqualTo(expected);
	assertThat(mBeanServer.isRegistered(new ObjectName("com.zaxxer.hikari:type=PoolConfig (" + poolName + ")")))
		.isEqualTo(expected);
}
```

Full logs:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-jdbc.org.springframework.boot.jdbc.autoconfigure.DataSourceJmxConfigurationTests.out.log`,
`.../module_spring-boot-jdbc.org.springframework.boot.jdbc.autoconfigure.LazyConnectionDataSourceCo-d6238dc7f3be.out.log`

## Root cause (hypothesis, not confirmed by attaching a debugger)

Not root-caused to a specific `native-builtins/src/jmx.rs` line in this
pass. `docs/internal/CRATONVM_BUGS/BUG-TC0622-jmx-mbean-registration-missing.md`
(FIXED 2026-06-23) documents that CratonVM's synthetic platform
`MBeanServer` (`native-builtins/src/jmx.rs`) *does* correctly register beans
(that fix's own probe: `isRegistered` and `getMBeanCount` both see freshly
registered beans, only `queryNames`/`queryMBeans` enumeration was broken and
that part is fixed) — so a blanket "registration is a no-op" theory is
already refuted for the general case, and this looks like a narrower,
HikariCP-specific gap rather than a regression of TC0622.

The most likely candidate given the checkpoint/restore reflection failure
found in the same class family (see the sibling doc
`jdbc-hikariconfig-copystateto-field-access-cluster.md`, filed same day from
this same rerun): HikariCP's own MBean-registration code path
(`HikariPool`/`HikariConfigMXBean` setup, invoked lazily on first
`getConnection()`) does its own reflection over pool/config internals before
calling `MBeanServer.registerMBean`. HikariCP's `PoolBase`/`HikariPool`
constructor wraps that MBean setup in a caught exception (Hikari logs a
warning and continues rather than failing pool startup on JMX registration
errors) — which would explain why `hikariAutoConfiguredCanUseRegisterMBeans`
still gets a working, connectable `HikariDataSource` (`hasSingleBean`,
`getConnection()` all pass) while the MBean assertions alone fail: if
CratonVM's reflection throws partway through Hikari's registration
bookkeeping, the pool still comes up fine but no MBean is registered.
**This is not confirmed** — the actual registration code path inside
HikariCP was not traced in this pass, and no log line in `.out.log`/`.err.log`
shows a caught-and-swallowed exception (Hikari's own logger output is not
captured by the suite runner). Confirming this requires either a
`CRATONVM_DBG_*`-instrumented rebuild that traces `MBeanServer.registerMBean`
calls, or a standalone repro that surfaces Hikari's internally-caught
exception (e.g. temporarily patching HikariCP to rethrow, or attaching a
JUL handler to `com.zaxxer.hikari.pool.HikariPool`'s logger).

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-jdbc` | `org.springframework.boot.jdbc.autoconfigure.DataSourceJmxConfigurationTests` (4 of 8 tests) |
| `module/spring-boot-jdbc` | `org.springframework.boot.jdbc.autoconfigure.LazyConnectionDataSourceConfigurationTests` (2 of 6 tests) |
