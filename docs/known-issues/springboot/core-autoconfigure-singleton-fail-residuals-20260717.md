# `core/spring-boot-autoconfigure` 2026-07-17 rerun: 5 unrelated single-class FAILs, bundled here for record

**Status: OPEN — found 2026-07-17**

Five classes from this triage batch each fail for a distinct reason with no
overlap with any other class in the batch or with each other — bundled into
one doc per this session's discretion (each gets its own subsection rather
than an artificially-forced shared "cluster"), instead of either 5 tiny
separate stub files or silently dropping them. Root causes below are
grounded in the actual stack traces and (where checked) the real Spring Boot
test source in this worktree, but none were traced to a specific CratonVM
source file/line this session — all are hypotheses of varying confidence,
explicitly marked as such.

## 1. `AutoConfigurationImportSelectorTests` — exclusion-validation `IllegalStateException` never thrown for a classpath-present non-autoconfiguration class

2/19 tests fail, both with the identical shape:

```
=> java.lang.AssertionError:
Expecting code to raise a throwable.
       org.springframework.boot.autoconfigure.AutoConfigurationImportSelectorTests.nonAutoConfigurationPropertyExclusionsWhenPresentOnClassPathShouldThrowException(AutoConfigurationImportSelectorTests.java:192)
```

(and the identically-shaped `nonAutoConfigurationClassNameExclusionsWhenPresentOnClassPathShouldThrowException`, line 185).

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard1/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.AutoConfigurationImportSelectorTests.out.log`

**Test source** (`AutoConfigurationImportSelectorTests.java:183-193`, this worktree):
both tests set `spring.autoconfigure.exclude`/pass a `@EnableAutoConfiguration(exclude=...)`
naming `AutoConfigurationImportSelectorTests.TestConfiguration` — a real
nested class of the test itself, present on the classpath, but *not* a valid
auto-configuration class — and assert `AutoConfigurationImportSelector`
throws `IllegalStateException` because the excluded name is present-but-not-an-autoconfiguration.
Real HotSpot throws; CratonVM does not.

**Hypothesis (unconfirmed):** the exclusion-validation logic's classpath
presence check (`ClassUtils.isPresent`/`Class.forName` on the given
fully-qualified name) returns/behaves as if the class is *not* present for
this specific name shape — a nested class of the currently-running test
class, referenced by its binary name string
(`...AutoConfigurationImportSelectorTests.TestConfiguration`) — causing the
validation to silently skip raising the exception instead of correctly
finding the class and rejecting the exclusion. Not verified against
CratonVM's `Class.forName`/nested-class-name-resolution source this
session.

## 2. `ConditionalOnJndiTests` — JNDI-conditional bean never registered even when JNDI location is (test-)available

2/6 tests fail:

```
=> java.lang.AssertionError:
Expecting:
 <Started application [...] beanDefinitionCount = 4]>
to have a single bean of type:
 <java.lang.String>
but found no beans of that type
       org.springframework.boot.autoconfigure.condition.ConditionalOnJndiTests.lambda$jndiAvailable$0(ConditionalOnJndiTests.java:86)
```

(and the analogous `jndiLocationBound`, line 101). Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard1/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.condition.ConditionalOnJndiTests.out.log`

**Hypothesis (unconfirmed):** these two tests install a mock/test
`InitialContextFactory` (via `TestableInitialContextFactory` or similar, the
standard Spring `@ConditionalOnJndi` test pattern) so that `@ConditionalOnJndi`'s
runtime JNDI-availability probe (a real `new InitialContext()` lookup) should
succeed and the guarded bean should register. On CratonVM, the condition
evaluates as if JNDI is unavailable (or the lookup fails), so the guarded
`String` bean never registers — a JNDI (`javax.naming`) support gap on
CratonVM, most likely in `InitialContext` construction/lookup rather than
in `@ConditionalOnJndi` itself (Spring's own condition class doesn't appear
in the trace, only the test's own assertion). Not traced further.

**Corroborating evidence (separate triage batch, same rerun):**
`module/spring-boot-jms`'s `JndiConnectionFactoryAutoConfigurationTests`
independently fails 4/6 tests the same day with the identical shape — e.g.
`detectWithXAConnectionFactory`:
```
=> java.lang.AssertionError:
Expecting:
 <Started application [...] beanDefinitionCount = 4]>
to have a single bean of type:
 <jakarta.jms.ConnectionFactory>
but found no beans of that type
```
Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-jms.org.springframework.boot.jms.autoconfigure.JndiConnectionFactoryAutoCon-3ea18dcd1d56.err.log`
(and matching `.out.log`). This test uses the exact same JNDI test-mock
plumbing as `ConditionalOnJndiTests` — `TestableInitialContextFactory`
installed via the `Context.INITIAL_CONTEXT_FACTORY` system property plus a
`JndiPropertiesHidingClassLoader` set as the thread context class loader in
`@BeforeEach` (`JndiConnectionFactoryAutoConfigurationTests.java:50-56`, this
worktree) — and every test that registers a JNDI-bound `ConnectionFactory`
via that mock and then expects auto-configuration to find it via a real
`new InitialContext()` lookup fails to find any bean, while
`detectNoAvailableCandidates` (asserting *no* bean when nothing is JNDI-bound)
passes. This independently confirms the hypothesis above from a second,
unrelated auto-configuration (`JndiConnectionFactoryAutoConfiguration` instead
of `@ConditionalOnJndi`), strengthening the case that this is a general
`javax.naming.InitialContext` lookup gap on CratonVM rather than something
specific to the `@ConditionalOnJndi` condition class. Still not traced to a
specific CratonVM `javax.naming` source location this session.

## 3. `NoSuchBeanDefinitionFailureAnalyzerTests` — qualifier text formatting mismatch in the failure-analysis message

1/11 tests fail — `failureAnalysisForUnmatchedQualifier`:

```
=> java.lang.AssertionError:
Expecting actual:
  "Parameter 0 of method consumer in ...QualifiedBeanConfiguration required a bean of type '...QualifiedBeanConfiguration$Thing' that could not be found.

The injection point has the following annotations:
	- @org.springframework.beans.factory.annotation.Qualifier(value="alpha")

The following candidates were found but could not be injected:
	- User-defined bean method 'producer' in 'NoSuchBeanDefinitionFailureAnalyzerTests.QualifiedBeanConfiguration'
"
to contain pattern:
  "@org.springframework.beans.factory.annotation.Qualifier\("*alpha"*\)"
       org.springframework.boot.autoconfigure.diagnostics.analyzer.NoSuchBeanDefinitionFailureAnalyzerTests.failureAnalysisForUnmatchedQualifier(NoSuchBeanDefinitionFailureAnalyzerTests.java:186)
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard1/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.diagnostics.analyzer.NoS-7106a1a9f67a.out.log`

The failure-analysis message actually generated is **substantively correct
and complete** (real qualifier value "alpha", real candidate bean listed) —
this is not a missing-feature failure, it's a **text-formatting mismatch**
against a regex the test expects: the assertion's expected pattern is
`@org.springframework.beans.factory.annotation.Qualifier\("*alpha"*\)` (a
`*`-wildcarded literal match, effectively expecting the annotation to be
rendered as `@Qualifier("alpha")` with quotes around the value), while the
actual text renders it unquoted as `@Qualifier(value="alpha")` — wait, the
actual text *does* show `value="alpha"` with quotes; the mismatch is
specifically the annotation-`toString()` rendering shape (`(value="alpha")`
vs. the pattern's expected `("alpha")`-without-`value=`-prefix form).
**Hypothesis (unconfirmed):** CratonVM's `Annotation.toString()` /
`AnnotationInvocationHandler`-equivalent renders a single-member annotation's
sole element with an explicit `value=` prefix where real HotSpot elides it
for the conventional `value()` element name — a real-JDK annotation-proxy
`toString()` formatting gap. Not verified against CratonVM's annotation-proxy
native source this session.

## 4. `SpringApplicationAdminJmxAutoConfigurationTests` — `MBeanServer` operations throw `IllegalArgumentException` instead of `javax.management.InstanceNotFoundException`

4/5 tests fail. Three surface the wrong exception type directly:

```
=> java.lang.IllegalArgumentException: InstanceNotFoundException: org.springframework.boot:type=Admin,name=SpringApplication
       org.springframework.boot.autoconfigure.admin.SpringApplicationAdminJmxAutoConfigurationTests.lambda$notRegisteredWhenThereAreNoMBeanExporter$0(...)
```

and the fourth explicitly asserts on the exception *type*:

```
=> java.lang.AssertionError:
Expecting actual throwable to be an instance of:
  javax.management.InstanceNotFoundException
but was:
  java.lang.IllegalArgumentException: InstanceNotFoundException: org.springframework.boot:type=Admin,name=SpringApplication
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard1/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.admin.SpringApplicationA-d4dd8b524340.out.log`

**Hypothesis (unconfirmed):** whatever CratonVM `MBeanServer` operation
backs these calls (`getAttribute`/`invoke` on a not-yet-registered
`org.springframework.boot:type=Admin,name=SpringApplication` MBean) detects
the missing registration and reports it via a plain
`IllegalArgumentException` carrying `"InstanceNotFoundException: <name>"` as
free text in the message, instead of throwing the real checked
`javax.management.InstanceNotFoundException` type the JMX API contract
requires. This module's `docs/internal/CRATONVM_BUGS/BUG-TC0622-jmx-mbean-registration-missing.md`
already documents CratonVM's synthetic JMX/`MBeanServer` implementation
(gated behind the default-on `experimental-jmx` feature,
`native-builtins/src/jmx.rs`) as an acknowledged **partial** subsystem (that
doc's specific finding is `queryNames`/`queryMBeans` returning empty, a
different symptom) — this is plausibly another gap in the same
partially-implemented synthetic `MBeanServer`, but the specific
wrong-exception-type mechanism here was not traced to a specific function in
`jmx.rs` this session.

## 5. `TaskExecutionAutoConfigurationTests` — context-propagation value not visible on a task-executor thread

1/36 tests fail — `asyncTaskExecutorWhenContextPropagationIsEnabledShouldRegisterBean`:

```
=> java.lang.AssertionError:
Expecting actual:
  "task-1 null"
to end with:
  "from-context"
       org.springframework.boot.autoconfigure.task.TaskExecutionAutoConfigurationTests.lambda$asyncTaskExecutorWhenContextPropagationIsEnabledShouldRegisterBean$0(TaskExecutionAutoConfigurationTests.java:288)
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.task.TaskExecutionAutoCo-c38a19897db4.out.log`

**Hypothesis (unconfirmed):** this test verifies Java's
`io.micrometer.context`/JDK `ScopedValue`-or-`ThreadLocal`-based context
propagation (Spring Boot 4's `ContextPropagation` support) actually carries
a value set on the submitting thread into the task executed on a pooled
`task-1` thread. The observed `"task-1 null"` (the propagated value reads as
literal `null` on the worker thread instead of `"...from-context"`) points
at CratonVM's `TaskDecorator`/context-propagation-wrapping mechanism (or the
underlying `ThreadLocal`/`ScopedValue` propagation primitive it depends on)
not actually carrying the value across the executor-thread hop. Not traced
to a specific CratonVM source location this session; the general area
(thread-local/root propagation across executor-managed threads) has prior,
unrelated CratonVM bug history in this codebase (e.g. thread-mirror
snapshot roots), but no direct connection to this specific test was
confirmed.

## Affected classes

| Module | Class |
|---|---|
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.AutoConfigurationImportSelectorTests` |
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.condition.ConditionalOnJndiTests` |
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.diagnostics.analyzer.NoSuchBeanDefinitionFailureAnalyzerTests` |
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.admin.SpringApplicationAdminJmxAutoConfigurationTests` |
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.task.TaskExecutionAutoConfigurationTests` |
| `module/spring-boot-jms` | `org.springframework.boot.jms.autoconfigure.JndiConnectionFactoryAutoConfigurationTests` (corroborating evidence for item 2, see Update in that section) |
