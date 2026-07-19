# `@Nested` class construction fails under an isolated `ModifiedClassPathClassLoader` — outer-instance argument type mismatch

**Status: FIXED — resolved 2026-07-19, same day as filed.** Residual of
[`modifiedclasspath-aether-network-hang-cluster-FIXED.md`](modifiedclasspath-aether-network-hang-cluster-FIXED.md).
Filed as OPEN after observing the failure against a binary built before
merging ~106 commits of `origin/dev` drift into the fix branch; after that
merge (which pulled in unrelated, already-landed fixes from other concurrent
sessions working the same repo) and a rebuild, `ConnectionFactoryUnwrapperTests`
passes **12/12** including `Unwrap.unwrapWithoutJmsPoolOnClasspath()`. Not
independently root-caused — resolved as a side effect of the drift merge,
exact fixing commit not identified. Kept here (rather than deleted) as a
record of the symptom and hypothesis in case it regresses.

## Symptom

`module/spring-boot-jms`'s `ConnectionFactoryUnwrapperTests.Unwrap
.unwrapWithoutJmsPoolOnClasspath()` (a `@Test` method on a `@Nested` class,
under a method-level `@ClassPathExclusions("pooled-jms-*")`) fails:

```
java.lang.IllegalArgumentException: argument type mismatch
	at org.junit.platform.commons.util.ReflectionUtils.newInstance(ReflectionUtils.java:589)
	at org.junit.jupiter.engine.execution.ConstructorInvocation.proceed(ConstructorInvocation.java:57)
	at org.junit.jupiter.engine.execution.InvocationInterceptorChain$ValidatingInvocation.proceed(InvocationInterceptorChain.java:124)
	at org.junit.jupiter.api.extension.InvocationInterceptor.interceptTestClassConstructor(InvocationInterceptor.java:94)
	...
	at org.junit.jupiter.engine.descriptor.NestedClassTestDescriptor.instantiateTestClass(NestedClassTestDescriptor.java:110)
	at org.junit.jupiter.engine.descriptor.ClassBasedTestDescriptor.instantiateAndPostProcessTestInstance(ClassBasedTestDescriptor.java:332)
```

The other 11/12 test methods in the class pass. Reproduced with
`cratonvm.exe` (`sb-runner` harness, real classpath) in worktree
`CratonVM-aether-modifiedclasspath-20260718-019f753a`:
`SBRUNNER_RESULT tests=12 failed=1`.

## Scope

Grepped every class in the affected-classes table of the parent (now-fixed)
doc for `@Nested` + `@ClassPathExclusions`/`@ClassPathOverrides` combined —
`ConnectionFactoryUnwrapperTests` is the **only** one. This is a narrow
residual, not a blocker for retiring the parent doc.

## Root-cause hypothesis (not confirmed by a debugger attach)

`ReflectionUtils.newInstance` here is JUnit5's mechanism for constructing a
`@Nested` inner class's test instance: `Unwrap`'s (implicit, compiler-
generated) constructor takes the enclosing `ConnectionFactoryUnwrapperTests`
instance as its sole parameter (`Unwrap(ConnectionFactoryUnwrapperTests
outer)`), and JUnit5 calls `Constructor.newInstance(outerInstance)`.
`IllegalArgumentException: argument type mismatch` is the JVM's standard
signal that the *runtime type* of the supplied argument doesn't match the
constructor parameter's *declared type* — a classic split-classloader
identity mismatch: two `Class` objects with the same binary name
(`ConnectionFactoryUnwrapperTests`) but loaded by two different loader
instances are never assignment-compatible, by design (JVMS).

The already-fixed recursion bug's fix (`preload_isolated_loader_supertypes`,
see the parent doc) explicitly and narrowly eagerly resolves only a newly-
defined class's **superclass and interfaces** through its defining isolated
loader — not other constant-pool type references such as a `@Nested` class's
compiler-synthesized outer-instance field/constructor-parameter type. That
relationship isn't a hierarchy edge (`extends`/`implements`), so the fix's
supertype-walk doesn't cover it. The likely gap: when `Unwrap` (defined
through the isolated `ModifiedClassPathClassLoader`, since the whole test
class was reloaded through it on the recursion's second pass) has its
constructor's parameter type (`ConnectionFactoryUnwrapperTests`) resolved —
whether at class-definition/verification time or at JUnit's reflective
`Constructor.newInstance()` call time — that resolution may be going through
a different path than the one used to construct/obtain the actual outer
instance JUnit passes in, ending up with two different `Class` objects for
the same binary name.

## Suggested next step

Confirm which of the two `ConnectionFactoryUnwrapperTests` `Class` objects
(the constructor parameter's expected type vs. the actual outer instance's
runtime type) differs in identity — e.g. by adding a temporary trace at the
`IllegalArgumentException` site or via `ctx.class_id_defined_by_loader_exact`
lookups for both. If confirmed, the fix is likely either (a) extending the
isolated-loader eager-resolution mechanism to also cover the synthetic
outer-instance parameter of a `@Nested` class at definition time, or (b)
ensuring JUnit5's own outer-instance-construction path (`TestInstancesProvider`
→ `NestedClassTestDescriptor.instantiateTestClass`) resolves the outer
instance's `Class` through the *same* defining loader as the nested class
rather than through a global/cached path.
