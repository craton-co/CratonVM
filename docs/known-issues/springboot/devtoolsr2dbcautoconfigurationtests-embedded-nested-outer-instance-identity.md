# `DevToolsR2dbcAutoConfigurationTests$Embedded` — `@Nested` outer-instance argument type mismatch

**Status: OPEN — investigated 2026-07-23, root cause substantially understood,
safe fix not yet found.** Same underlying pattern as
[`connectionfactoryunwrappertests-nested-outer-instance-identity-FIXED.md`](../../internal/fixed-suite-bugs/springboot/connectionfactoryunwrappertests-nested-outer-instance-identity-FIXED.md)
(`module/spring-boot-jms`), which was closed there as a side effect of an
unrelated `dev` merge, root cause never independently confirmed. This is a
fresh, independently-confirmed sighting of the same class of bug in a
different module.

## Symptom

`module/spring-boot-devtools`'s `DevToolsR2dbcAutoConfigurationTests`: all 5
test methods in the `@Nested @ClassPathExclusions("r2dbc-pool*.jar") class
Embedded extends Common` fail (the sibling `Pooled` nested class, with no
`@ClassPathExclusions`, passes all 5):

```
java.lang.IllegalArgumentException: argument type mismatch
	at org.junit.platform.commons.util.ReflectionUtils.newInstance(ReflectionUtils.java:589)
	at org.junit.jupiter.engine.execution.ConstructorInvocation.proceed(ConstructorInvocation.java:57)
	...
	at org.junit.jupiter.engine.descriptor.NestedClassTestDescriptor.instantiateTestClass(NestedClassTestDescriptor.java:110)
	at org.junit.jupiter.engine.descriptor.ClassBasedTestDescriptor.instantiateAndPostProcessTestInstance(ClassBasedTestDescriptor.java:332)
```

Reproduced deterministically (10/10 runs) with the suite harness
(`run-spring-boot-suite.ps1`), `SBRUNNER_RESULT tests=10 failed=5`, both via
the official 2026-07-23 429-class rerun and via repeated targeted
single-class reruns in a fresh worktree (`fix/springboot-devtools-fullfix-20260723`).

## Root cause (confirmed via targeted native tracing)

`ModifiedClassPathExtension` (Spring Boot's JUnit5 `@ClassPathExclusions`
fork mechanism) does **not** intercept constructor invocation — only
`@Test`/`@BeforeEach`/etc. method invocation. Test **class instantiation**
(`ClassBasedTestDescriptor.instantiateAndPostProcessTestInstance` /
`NestedClassTestDescriptor.instantiateTestClass`) happens as part of a
separate JUnit lifecycle phase, and — confirmed via CratonVM-side tracing
added temporarily during this investigation (`CRATONVM_DBG_UCLDEFINE`,
`CRATONVM_DBG_COERCE`, and Java-side thread/classloader prints in
`ModifiedClassPathExtension`/`ModifiedClassPathClassLoader`, all since
reverted) —  two *separate* `Class` definitions of
`DevToolsR2dbcAutoConfigurationTests` exist within one JVM process:

1. An early, pre-fork definition, resolved via the outer harness's own
   initial (non-isolated) `SbRunner.main` → `Class.forName(fqcn, false,
   appClassLoader)` → `outerClass.getDeclaredClasses()` walk that discovers
   `Embedded`/`Pooled`/`Common` as part of building the full JUnit
   `TestDescriptor` tree *before any test method runs, before any fork ever
   happens*. This copy is **not** registered under any CratonVM
   loader-namespace id (`class_manager`'s `get_loader_id` returns `None` for
   it — confirmed via a temporary trace on `loader_namespace_id`/
   `register_defining_loader`/`ucl_try_define_local_class`).
2. A second, freshly-isolated definition, resolved through
   `ModifiedClassPathClassLoader` (confirmed via `classloader_real.rs`'s
   `loadClass`-native gate firing with `isolated=true`, correctly minting a
   real `UserDefined` loader-namespace id) — this is the copy the *actual*
   outer test instance JUnit constructs at execution time is made from.

`Embedded`'s constructor (`near` in CratonVM's `coerce_arg_strict`) is
**declaration (1)** — JUnit reuses the pre-fork `Class<Embedded>` reference
for the constructor/declaring-class lookup — while the outer instance JUnit
actually passes as the constructor argument is **declaration (2)**. Both are
the textually-identical source class, but CratonVM tracks them as two
distinct `ClassId`s, so `is_subclass(arg_cid, expected_cid)` correctly (per
its own contract) rejects them as unrelated, throwing
`IllegalArgumentException: argument type mismatch`.

The `class_id_by_name_near`/`loader_namespace_id`/`defining_loader_for`
machinery already in the codebase (used successfully for the analogous
`Class.getDeclaringClass()`/`getEnclosingClass()` case, see
`declaring_class_loader_aware` in `native-builtins/src/lang_class.rs`) does
**not** help here because it depends on `near` (`Embedded`) itself having a
registered loader namespace — but `near`, being definition (1), has none.

## Why this is hard to fix safely

This session tried two fix strategies, both unsuccessful or unsafe:

1. **Eagerly preload the outer class through the isolated loader** when
   defining a non-static `@Nested` inner class (mirroring the existing
   super/interface preload, `preload_isolated_loader_supertypes` in
   `native-builtins/src/lang_system.rs`), so `class_id_by_name_near` would
   find a same-loader match. This correctly registers a fresh, isolated
   outer-class copy (confirmed loader-namespace id assigned) — but
   `class_id_by_name_near`'s lookup is keyed on `near`'s (still
   unregistered) namespace, so `expected_cid` still resolved to definition
   (1) via the global fallback. No effect on the failure. (Reverted —
   harmless but useless without also fixing `near`'s own registration,
   which is the deeper problem below.)

2. **Relax the argument-assignability check** for exactly this shape
   (constructor's first parameter is the compiler-synthesized `this$0` outer
   reference, detected via the class's own `InnerClasses` self-entry,
   non-static) to accept a same-named class regardless of which of the two
   loader-tracked copies defined it. This *did* make
   `ReflectionUtils.newInstance` succeed — but then caused a
   `java.lang.StackOverflowError` deep inside JUnit's own
   `CompositeTestExecutionListener` failure-reporting path, which itself
   recurses trying to log the overflow, producing an unbounded retry loop
   (observed hang at 900s). Most likely explanation: accepting the
   mismatched-loader outer instance lets construction proceed, but `static`
   state on `Common`/`Embedded` (e.g. the `shutdowns` list) is **not**
   shared between the two loader-tracked copies (JVMS: statics are per
   defining-class, and these are two distinct `ClassId`s) — so the test
   body's interaction with that divergent static state likely triggers the
   overflow. **Reverted** — a clean, understood test failure is much safer
   than a hang that could mask other results in a shared suite run.

## Suggested next steps (not attempted this session)

- The real fix is almost certainly on the JUnit-resolution side: prevent
  `NestedClassTestDescriptor`'s constructor-invocation path from reusing the
  pre-fork `Class<Embedded>` reference at all, so **both** the declaring
  class and the outer instance are resolved through the *same* (isolated,
  post-fork) definition. This may require either (a) making CratonVM's
  `outerClass.getDeclaredClasses()` implementation re-resolve nested members
  through the *current* thread context classloader rather than returning a
  cached/first-loaded set, or (b) confirming whether this is inherent to
  how `ModifiedClassPathExtension`'s `runTest()` nested `Launcher` discovers
  a `[nested-class:X]` `UniqueId` segment (worth attaching a Java debugger
  or adding JUL `FINE`-level tracing to `org.junit.platform` to see exactly
  which `Class` object `NestedClassSelectorResolver` hands to the
  `TestInstanceFactory`).
- Whichever fix is chosen, it needs a way to verify it doesn't just move
  the loader mismatch into the `Common`-declared static field access itself
  (i.e. don't declare victory on `ReflectionUtils.newInstance` succeeding
  alone — run the full 5-test class and confirm assertions against
  `shutdowns` also pass, not just that construction doesn't throw).

## Reproduce

```powershell
$env:JAVA_HOME = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
$exe = "<worktree>\target\release\cratonvm.exe"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Exe $exe -JdkHome $env:JAVA_HOME `
  -ClassList <tsv with header 'module\tclass', row 'module/spring-boot-devtools\torg.springframework.boot.devtools.autoconfigure.DevToolsR2dbcAutoConfigurationTests'> `
  -RunName r2dbc-repro -Parallel 1 -TimeoutSec 300
```

`CRATONVM_DBG_COERCE=1` (existing flag in `native-builtins/src/lang_class.rs`)
prints the expected/arg class ids and loader ids at the rejection site.
