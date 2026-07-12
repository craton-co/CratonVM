# Spring bean destroy-method resolution fails with "Invalid destruction signature" — 34 classes across many modules

**Status: OPEN, characterized (Spring-side call site pinned; CratonVM-side
reflection defect not yet isolated). Severity: HIGH (broad).**

Found while triaging `FAIL`s from the first full Spring Boot suite run (see
[[project_spring_boot_suite_runner_20260711]]). At least **34 test classes**
across `spring-boot-micrometer-metrics` (bulk of the cluster — every metrics
exporter: OTLP, New Relic, Graphite, StatsD, Stackdriver, KairosDB, JMX,
Humio, …), `spring-boot-jdbc` (`EmbeddedDataSourceConfiguration`),
`spring-boot-r2dbc`/`spring-boot-data-r2dbc`, `spring-boot-hazelcast`,
`spring-boot-data-jpa`, `spring-boot-data-rest`, `spring-boot-jooq`,
`spring-boot-flyway`, and `core/spring-boot-autoconfigure`
(`TaskSchedulingAutoConfigurationTests`) fail identically:

```
org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'X' defined in ...: Invalid destruction signature
	at org.springframework.beans.factory.support.AbstractAutowireCapableBeanFactory.doCreateBean(AbstractAutowireCapableBeanFactory.java:647)
```

## Spring-side mechanism (confirmed by decompiling `spring-beans-7.0.7.jar`)

`AbstractAutowireCapableBeanFactory.doCreateBean` calls
`registerDisposableBeanIfNecessary(beanName, bean, mbd)` after successful
bean instantiation+population — this constructs a `DisposableBeanAdapter`,
which reflectively resolves the bean's destroy method (either an explicit
`@Bean(destroyMethod=...)` name, or Spring's *inferred* convention: look for
a public no-arg `close()`/`shutdown()`). **Any** exception thrown during that
resolution is caught by `doCreateBean`'s handler at bytecode offset 442 and
rewrapped as `BeanCreationException("...", "Invalid destruction signature", cause)`
— so this message is a generic catch-all, not a specific Spring validation
failure. Confirmed by `javap -c` on
`AbstractAutowireCapableBeanFactory.class`: the handler wraps whatever
`registerDisposableBeanIfNecessary` threw, discarding the original exception
type from the visible message text (only `cause` retains it, and `SbRunner`'s
default JUnit-Platform failure printer here doesn't unwrap it — a deeper
capture with `-Dcraton.trace=1`-style instrumentation or a direct
`e.getCause()` dump is needed to see the exact reflection exception CratonVM
throws underneath).

## Why this points at CratonVM, not Spring or the test fixtures

The affected bean types share no code in common — third-party datasource
pools, R2DBC connection factories, Hazelcast instances, and a dozen
independent Micrometer registry implementations — but nearly all rely on
`close()`/`shutdown()` inherited from a JDK interface
(`java.lang.AutoCloseable`/`java.io.Closeable`/`java.util.concurrent.ExecutorService`)
rather than a method declared directly on the concrete bean class. This
strongly suggests CratonVM's reflective destroy-method lookup
(`Class.getMethod`/`getDeclaredMethod`-family, invoked internally by
Spring's `ClassUtils.getInterfaceMethodIfPossible` /
`DisposableBeanAdapter` construction path) fails to resolve an
interface-inherited method that real HotSpot resolves fine — the same
general shape as previously-fixed CratonVM reflection gaps in this codebase
(loader-identity mismatches, `invoke_virtual` native-dispatch-cache quirks
resolving by name+descriptor alone; see
[[reference_invoke_virtual_native_dispatch_cache_quirk]]), but not yet
pinned to a specific native function for this exact call shape.

## Next step

Reproduce standalone (`DisposableBeanAdapter` against a plain bean
implementing only `AutoCloseable.close()`, no Spring context) with
`CRATONVM_DBG_REFLECT`-style tracing (or a temporary `e.getCause().printStackTrace()`
patch in a local `SbRunner` build) to capture the actual underlying
exception class/message CratonVM throws, then correlate to the specific
native reflection function. `TaskSchedulingAutoConfigurationTests` (small,
fast, `core/spring-boot-autoconfigure`) is the smallest repro in the cluster
— use it first rather than the heavier Micrometer/R2DBC classes.

## Repro

```powershell
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -ClassList <TSV: 'core/spring-boot-autoconfigure<TAB>org.springframework.boot.autoconfigure.task.TaskSchedulingAutoConfigurationTests'> `
  -Start 1 -Count 1 -Exe <cratonvm exe>
```
