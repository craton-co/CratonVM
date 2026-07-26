# `DevToolsR2dbcAutoConfigurationTests$Embedded` — `@Nested` outer-instance argument type mismatch

**Status: FIXED — confirmed 2026-07-26.** Same underlying pattern as
[`connectionfactoryunwrappertests-nested-outer-instance-identity-FIXED.md`](connectionfactoryunwrappertests-nested-outer-instance-identity-FIXED.md)
(`module/spring-boot-jms`). Not independently root-caused this session either
(dev moves fast — 657+ commits landed between this doc's last update on
`fix/aot-cluster-20260726` and this verification pass, from many concurrent
sessions' classloader/loader-identity fixes across the repo); confirmed
RESOLVED as a side effect of general `dev` drift, exact fixing commit not
isolated. Kept here (rather than deleted) as a record of the symptom,
root-cause investigation, and fix attempts, in case it regresses.

## Symptom (historical)

`module/spring-boot-devtools`'s `DevToolsR2dbcAutoConfigurationTests`: all 5
test methods in the `@Nested @ClassPathExclusions("r2dbc-pool*.jar") class
Embedded extends Common` failed (the sibling `Pooled` nested class, with no
`@ClassPathExclusions`, passed all 5):

```
java.lang.IllegalArgumentException: argument type mismatch
	at org.junit.platform.commons.util.ReflectionUtils.newInstance(ReflectionUtils.java:589)
	at org.junit.jupiter.engine.execution.ConstructorInvocation.proceed(ConstructorInvocation.java:57)
	...
	at org.junit.jupiter.engine.descriptor.NestedClassTestDescriptor.instantiateTestClass(NestedClassTestDescriptor.java:110)
	at org.junit.jupiter.engine.descriptor.ClassBasedTestDescriptor.instantiateAndPostProcessTestInstance(ClassBasedTestDescriptor.java:332)
```

Previously reproduced deterministically (10/10 runs) with the suite harness
(`run-spring-boot-suite.ps1`), `SBRUNNER_RESULT tests=10 failed=5`.

## Verification (2026-07-26)

Checked out `dev` at `887cd01fa4b5b9e9498aad10442e1cd8147e4763` in a fresh
worktree (`fix/devtools-r2dbc-nested-20260726`), built `cratonvm-cli`
release, and ran `DevToolsR2dbcAutoConfigurationTests` via
`run-spring-boot-suite.ps1` (pwsh on the Linux build host) against a Spring
Boot 4.1.0-SNAPSHOT checkout with the `module/spring-boot-devtools`
classpath freshly regenerated (`cratonvmTestCp` Gradle task):

```
SBRUNNER_RESULT tests=10 failed=0 aborted=0 skipped=0 containersFailed=0
```

All 4 containers (outer class + `Common`-derived `Embedded`/`Pooled` nested
classes + engine) succeeded, all 10 tests (5 `Embedded` + 5 `Pooled`)
passed. Repeated **5/5 times** (`r2dbc-repro-v1`, `v2`, `rep1`-`rep4`) with
identical results — no flakiness.

Also re-verified the sibling `module/spring-boot-jms`
`ConnectionFactoryUnwrapperTests` (the doc this one mirrors) against the same
binary: `SBRUNNER_RESULT tests=12 failed=0 aborted=0 skipped=0
containersFailed=0` — still holds.

Grepped the full Spring Boot checkout (`core`, `module`, `cli`,
`configuration-metadata`, `loader`, `test-support`) for every class combining
`@Nested` with `@ClassPathExclusions`/`@ClassPathOverrides` — these two
classes (`DevToolsR2dbcAutoConfigurationTests`, `ConnectionFactoryUnwrapperTests`)
are the **only** matches in the whole suite, so there is no remaining
residual of this bug class elsewhere.

## Root cause (as understood before the fix landed — kept for history)

`ModifiedClassPathExtension` (Spring Boot's JUnit5 `@ClassPathExclusions`
fork mechanism) does **not** intercept constructor invocation — only
`@Test`/`@BeforeEach`/etc. method invocation. Test **class instantiation**
(`ClassBasedTestDescriptor.instantiateAndPostProcessTestInstance` /
`NestedClassTestDescriptor.instantiateTestClass`) happens as part of a
separate JUnit lifecycle phase.

Decompiling `junit-jupiter-engine`'s `ClassSelectorResolver` (via `javap`)
confirmed JUnit's own resolution bytecode is correct —
`resolveStandaloneClassUniqueId` calls `ReflectionSupport.tryToLoadClass(name)`
(no explicit loader), which `ClassLoaderUtils.getDefaultClassLoader()`
resolves to `Thread.currentThread().getContextClassLoader()`.
`resolveNestedClassUniqueId` then finds the nested class via
`ReflectionSupport.findNestedClasses(parentTestClass, predicate)` — i.e.
`parentTestClass.getDeclaredClasses()` — on whatever `Class` the parent
segment resolved to. So architecturally the nested class SHOULD end up
under the same loader as its freshly-resolved parent.

Native tracing (`CRATONVM_DBG_COERCE` + a temporary trace in
`native_class_get_declared_classes`) showed: `near` (`Embedded`'s declaring
class used for the constructor invocation) and `expected` (the constructor's
outer-instance parameter type) were both genuinely, correctly registered
under `ClassLoaderId::Application`; the only mismatched side was `arg` — the
actual outer *test instance* object JUnit constructs at execution time —
which was a freshly isolated (`ModifiedClassPathClassLoader`, real
`UserDefined` namespace id) instance. This meant JUnit5 itself, for
constructing the actual `@Nested` outer test instance, used a different
`Class` reference than the one its own discovery-phase `getDeclaredClasses()`
walk produced.

## Fix attempts this session (2026-07-23 → 2026-07-24, before this doc's closure)

1. **Eagerly preload the outer class through the isolated loader** at
   `@Nested` class-definition time. No effect — reverted.

2. **Relax the argument-assignability check** to accept a same-named class
   across the two loader-tracked copies for exactly the outer-instance
   parameter shape. Made `ReflectionUtils.newInstance` succeed but caused
   a `java.lang.StackOverflowError` cascade elsewhere (divergent `static`
   state between the two copies). **Reverted.**

3. **Add a genuine reverse loader-namespace lookup**
   (`loader_object_for_namespace_id` in `native-builtins/src/classloader.rs`)
   and wire it into `Class.getDeclaredClasses()`'s existing (but previously
   unreliable) loader-driving fallback in `native-builtins/src/lang_class.rs`.
   A genuine, verified-safe architectural fix (zero regressions across a
   51-class module regression); did not on its own close this bug, but was
   kept and merged, and — combined with unrelated loader-identity work
   landed by other concurrent sessions over the following two days —
   evidently closed the remaining gap.

## Reproduce (for regression checking)

```powershell
$env:JAVA_HOME = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
$exe = "<worktree>\target\release\cratonvm.exe"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Exe $exe -JdkHome $env:JAVA_HOME `
  -ClassList <tsv with header 'module\tclass', row 'module/spring-boot-devtools\torg.springframework.boot.devtools.autoconfigure.DevToolsR2dbcAutoConfigurationTests'> `
  -RunName r2dbc-repro -Parallel 1 -TimeoutSec 300
```

`CRATONVM_DBG_COERCE=1` (existing flag in `native-builtins/src/lang_class.rs`)
prints the expected/arg class ids and loader ids at the rejection site, if
this ever regresses.
