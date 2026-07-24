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

## Root cause (confirmed via bytecode analysis + targeted native tracing, updated 2026-07-24)

`ModifiedClassPathExtension` (Spring Boot's JUnit5 `@ClassPathExclusions`
fork mechanism) does **not** intercept constructor invocation — only
`@Test`/`@BeforeEach`/etc. method invocation. Test **class instantiation**
(`ClassBasedTestDescriptor.instantiateAndPostProcessTestInstance` /
`NestedClassTestDescriptor.instantiateTestClass`) happens as part of a
separate JUnit lifecycle phase.

**2026-07-24 correction to the original framing below:** decompiling
`junit-jupiter-engine`'s `ClassSelectorResolver` (via `javap`) confirmed
JUnit's own resolution bytecode is correct — `resolveStandaloneClassUniqueId`
calls `ReflectionSupport.tryToLoadClass(name)` (no explicit loader), which
`ClassLoaderUtils.getDefaultClassLoader()` resolves to
`Thread.currentThread().getContextClassLoader()` (verified: no hidden
static Class-by-name cache exists in `junit-platform-commons`'
`ReflectionUtils` — only a small primitive-name table, unrelated).
`resolveNestedClassUniqueId` then finds the nested class via
`ReflectionSupport.findNestedClasses(parentTestClass, predicate)` — i.e.
`parentTestClass.getDeclaredClasses()` — on whatever `Class` the parent
segment resolved to. So architecturally the nested class SHOULD end up
under the same loader as its freshly-resolved parent.

Fresh tracing (`CRATONVM_DBG_COERCE` + a temporary trace added to
`native_class_get_declared_classes`, since removed) showed:

- `near` (`Embedded`'s declaring class used for the constructor
  invocation) and `expected` (the constructor's outer-instance parameter
  type) are **both genuinely, correctly registered under
  `ClassLoaderId::Application`** — this is a real `class_manager` record
  (`cm.get_loader_id()` returns `Some(Application)`, not `None`
  defaulting to a display value of `2`, as originally suspected). In other
  words, `near`/`expected` are **not** an untracked/stale artifact; they
  are the ordinary, singular Application-loader copy of
  `Outer`/`Embedded` that both the ORIGINAL SbRunner-level discovery and
  (per this correction) JUnit's own `@Nested`-resolution genuinely land
  on.
- The **only** mismatched side is `arg` — the actual outer *test instance
  object* JUnit constructs at execution time — which is a freshly
  isolated (`ModifiedClassPathClassLoader`, real `UserDefined` namespace
  id) instance.
- A fix was added (`native-builtins/src/classloader.rs`'s new
  `loader_object_for_namespace_id` reverse lookup, wired into
  `native-builtins/src/lang_class.rs`'s `native_class_get_declared_classes`
  loader-driving fallback) that correctly and verifiably drives a fresh,
  properly-isolated `Embedded` definition when `getDeclaredClasses()` is
  called on the freshly-isolated outer `Class` — confirmed via tracing
  that this call **does** happen (5×, once per test method) and **does**
  produce a new, correctly-`UserDefined`-tracked `Embedded`. But that
  freshly-created `Embedded` is **not** what ends up as `near` for the
  constructor invocation JUnit actually performs — `near` stays the
  Application-loader copy across all 5 methods, unaffected by the fix.

This means the mismatch is **not** (as originally framed) "CratonVM
resolves the same class two different ways due to a loader-registration
gap it can fix locally" — it's that **JUnit5 itself, for constructing the
actual `@Nested` outer test instance, uses a different `Class` reference
than the one its own discovery-phase `getDeclaredClasses()` walk
produces** (or than CratonVM's `getDeclaredClasses()` is asked to
produce). Where exactly that second, isolated resolution comes from
inside JUnit5's execution phase (as opposed to its discovery phase) was
not pinned down this session — plausible candidates: `TestInstancesProvider`
independently re-resolving the outer class at instantiation time via
some other path, or `ModifiedClassPathClassLoader`'s own
`super.loadClass()` delegation chain behaving differently for the
already-loaded-by-app-loader case than assumed. Needs either a Java
debugger attach or JUL `FINE`-level tracing on `org.junit.platform`
(neither available on this session's box) to pin down precisely.

## Fix attempts this session (2026-07-23 → 2026-07-24)

1. **Eagerly preload the outer class through the isolated loader** at
   `@Nested` class-definition time (mirroring the existing super/interface
   preload, `preload_isolated_loader_supertypes` in
   `native-builtins/src/lang_system.rs`). No effect — reverted (superseded
   by the understanding above: `near` was never going to be the isolated
   copy regardless).

2. **Relax the argument-assignability check** to accept a same-named class
   across the two loader-tracked copies for exactly the outer-instance
   parameter shape. Made `ReflectionUtils.newInstance` succeed but caused
   a `java.lang.StackOverflowError` cascade elsewhere (divergent `static`
   state between the two copies — JVMS: statics are per-`ClassId`).
   **Reverted** — a clean, understood test failure beats a hang that could
   mask other results in a shared suite run.

3. **Add a genuine reverse loader-namespace lookup** (`loader_object_for_namespace_id`
   in `native-builtins/src/classloader.rs`) and wire it into
   `Class.getDeclaredClasses()`'s existing (but previously unreliable,
   since it depended on the narrowly-populated `defining_loader_for` side
   table) loader-driving fallback in `native-builtins/src/lang_class.rs`.
   This is a **genuine, verified-safe architectural fix** — confirmed via
   a full 51-class module regression to introduce zero regressions among
   the 47 previously-passing classes — and is worth keeping even though it
   does not, on its own, close this specific bug (see above: the gap is
   deeper than `getDeclaredClasses()`'s own loader fidelity). **Kept and
   merged.**

## Suggested next steps

- Attach a Java debugger (not available on this session's box) or add
  `-Djava.util.logging` `FINE`-level tracing to `org.junit.platform`
  packages to find exactly which code path constructs the isolated outer
  test instance at *execution* time, and why it doesn't reuse the same
  `Class` reference `getDeclaredClasses()`/discovery-phase resolution
  settled on.
- Whichever fix is chosen, verify it doesn't just move the loader mismatch
  into `Common`-declared static field access (e.g. `shutdowns`) — run the
  full 5-test class and confirm assertions pass, not just that
  construction doesn't throw.

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
