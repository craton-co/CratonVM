# Spring Boot core-autoconfigure singleton failures — fixed 2026-07-17

**Status: FIXED**

This record closes the five independent failures originally grouped in
`core-autoconfigure-singleton-fail-residuals-20260717.md`, including the
corroborating `spring-boot-jms` JNDI residual. They were reproduced from the
real Spring Boot fixture and fixed in CratonVM; no Spring source or fixture
was changed.

## Root causes and corrections

1. `AutoConfigurationImportSelectorTests` (2 failures)

   CratonVM's native override of Spring `ClassUtils.isPresent` reduced the
   operation to a global class probe. That bypassed Spring's loader-specific
   canonical nested-class fallback (`Outer.Inner` to `Outer$Inner`) and treated
   a present, non-auto-configuration test class as absent. The JSF safety
   exception remains, but every other call now executes Spring's bytecode
   implementation in `native-builtins/src/net_phase_e.rs`.

2. `ConditionalOnJndiTests` (2 failures) and
   `JndiConnectionFactoryAutoConfigurationTests` (4 residual failures)

   `InitialContext.lookup` intercepted the test-installed
   `java.naming.factory.initial` provider and used CratonVM's WildFly flat
   naming store instead. The native now honors the configured
   `InitialContextFactory`, including `InitialContext.getEnvironment`, while
   retaining the WildFly/Tomcat paths where they apply. See
   `native-builtins/src/wildfly_naming.rs`.

3. `NoSuchBeanDefinitionFailureAnalyzerTests` (1 failure)

   Annotation proxy rendering emitted the explicit `value=` label for a
   sole `value` element. `annotation_proxy_to_string` now matches Java's
   conventional rendering and elides that label only for a one-element
   annotation in `vm/src/vm/vm_exec.rs`.

4. `SpringApplicationAdminJmxAutoConfigurationTests` (4 failures)

   The synthetic MBean server converted missing registration into a text-only
   `IllegalArgumentException`, and `ManagementFactory.getPlatformMBeanServer`
   returned a new server for each call. The JMX bridge now creates the API's
   typed exceptions and holds one GC-rooted platform server across calls;
   collection remapping is wired through `native-builtins/src/jmx.rs`,
   `vm/src/memory/roots.rs`, and `vm/src/memory/gc.rs`.

5. `TaskExecutionAutoConfigurationTests` (1 failure)

   Micrometer found Spring Boot's method-scoped service descriptor but failed
   to read it: ServiceLoader called protected `findResources` rather than the
   custom loader's public `getResources`, then dropped the leading slash when
   converting its `file:/tmp/...` URL to a filesystem path. The service loader
   now uses `getResources` and preserves the URL path in
   `native-builtins/src/service_loader.rs`, so Micrometer installs the accessor
   and the task decorator propagates the existing ThreadLocal correctly.

## Verification

Fixture: `apps/spring-boot` with `apps/spring-boot-suite-runner` classpath
artifacts, executed using Java 25 and the uniquely named isolated binary
`cratonvm-springboot-core-autoconfigure-residuals-final-20260717`.

The following matrix passed in both normal JIT mode and with
`CRATONVM_DISABLE_JIT=1` (interpreter-only):

| Class | Tests | Result |
|---|---:|---|
| `AutoConfigurationImportSelectorTests` | 19 | 0 failed |
| `ConditionalOnJndiTests` | 6 | 0 failed |
| `NoSuchBeanDefinitionFailureAnalyzerTests` | 11 | 0 failed |
| `SpringApplicationAdminJmxAutoConfigurationTests` | 5 | 0 failed |
| `TaskExecutionAutoConfigurationTests` | 36 | 0 failed |
| `JndiConnectionFactoryAutoConfigurationTests` | 6 | 0 failed |

Total: 83 tests passed in each mode, with no aborted or failed containers.
