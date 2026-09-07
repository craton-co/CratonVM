# `org.springframework.core.test.tools.TestCompiler` silently fails to compile under JIT — 19-class AOT cluster

| | |
|---|---|
| **Status** | OPEN. Confirmed CratonVM-specific and JIT-specific. |
| **Scope** | 19 of the 56 classes common to all three GC arms in the 2026-09-07 full 2848-class Spring Framework suite run — every class that exercises `TestCompiler` (Spring's in-memory AOT-codegen compile harness) fails the identical way. |

## Symptom

Every affected class throws, from the exact same call site:

```
org.springframework.core.test.tools.CompilationException: Unable to compile source
	at org.springframework.core.test.tools.CompilationException.<init>(CompilationException.java:42)
	at org.springframework.core.test.tools.TestCompiler.compile(TestCompiler.java:316)
	at org.springframework.core.test.tools.TestCompiler.compile(TestCompiler.java:285)
	at org.springframework.core.test.tools.TestCompiler.compile(TestCompiler.java:263)
```

`TestCompilerTests` itself: `found=22 succ=3 fail=19`.

## The cluster

All 19 share `TestCompiler` as their common dependency, directly or via Spring's
AOT bean-registration code generators (which compile their generated source to
verify it, using the same class):

```
org.springframework.aop.scope.ScopedProxyBeanRegistrationAotProcessorTests
org.springframework.beans.factory.annotation.AutowiredAnnotationBeanRegistrationAotContributionTests
org.springframework.beans.factory.aot.BeanDefinitionMethodGeneratorTests
org.springframework.beans.factory.aot.BeanDefinitionPropertiesCodeGeneratorTests
org.springframework.beans.factory.aot.BeanDefinitionPropertyValueCodeGeneratorDelegatesTests
org.springframework.beans.factory.aot.BeanRegistrationsAotContributionTests
org.springframework.beans.factory.aot.CodeWarningsTests
org.springframework.beans.factory.aot.InstanceSupplierCodeGeneratorKotlinTests
org.springframework.beans.factory.aot.InstanceSupplierCodeGeneratorTests
org.springframework.context.annotation.CommonAnnotationBeanRegistrationAotContributionTests
org.springframework.context.annotation.ConfigurationClassPostProcessorAotContributionTests
org.springframework.context.aot.ApplicationContextAotGeneratorTests
org.springframework.core.test.tools.CompiledTests
org.springframework.core.test.tools.TestCompilerTests
org.springframework.orm.jpa.persistenceunit.PersistenceManagedTypesBeanRegistrationAotProcessorTests
org.springframework.orm.jpa.support.InjectionCodeGeneratorTests
org.springframework.orm.jpa.support.PersistenceAnnotationBeanPostProcessorAotContributionTests
org.springframework.test.context.aot.AotIntegrationTests
org.springframework.test.context.aot.TestContextAotGeneratorIntegrationTests
```

(`org.springframework.test.context.aot.TestClassScannerTests` and
`org.springframework.web.service.registry.HttpServiceProxyRegistrationAotProcessorTests`,
also in the original 56, look adjacent by name but were not confirmed to share
this exact cause — not re-checked individually.)

## Root cause, as far as isolated

`TestCompiler.compile()` (`spring-core-test`) calls a real `javax.tools.JavaCompiler`
task through a custom `DynamicJavaFileManager` that captures output classes in
memory rather than to disk:

```java
CompilationTask task = this.compiler.getTask(null, fileManager, problems, ...);
boolean result = task.call();
if (!result || problems.hasReportedErrors()) {
    throw new CompilationException(problems.elements, this.sourceFiles, this.resourceFiles);
}
```

**The `Problems` diagnostic collector is empty** — `CompilationException`'s own
message builder only prints an `Errors:`/`Warnings:` section when `problems`
is non-empty, and neither appears in the captured output. So `task.call()`
returned `false` (or the equivalent) without ever routing a diagnostic through
the `DiagnosticListener` — a silent failure inside the compile task itself,
not a normal javac error the source triggered. The source compiled in the
failing case (`Test.java` implementing a `PublicInterface` and calling a
package-private helper) is unremarkable Java.

**Confirmed CratonVM-specific**: real HotSpot (JDK 25, identical classpath,
identical harness) passes `TestCompilerTests` 22/22.

**Confirmed JIT-specific**: `--nojit` on the same CratonVM binary passes
`TestCompilerTests` 22/22 as well. JIT-on is the only failing configuration.

## Relationship to the already-fixed sibling bug

This shares its architecture — `DynamicJavaFileManager` / in-memory compile,
often combined with `@CompileWithForkedClassLoader` (visible in the stack via
`CompileWithForkedClassLoaderExtension.intercept`) — with
`aot-cglib-dynamicclassfileobject-illegalargumentexception-20260811-FIXED.md`,
whose root cause was the JIT compiling a callee **by class name** and handing
back a different loader's copy of `DynamicClassFileObject`, later thrown as
`IllegalArgumentException` from `javac`'s `inferBinaryName`.

**This is not the same symptom** — no `IllegalArgumentException` appears
anywhere in this stack, and the failure is silent (zero diagnostics) rather
than a thrown type-mismatch. Whether it is a residual of the same class of
JIT compile-by-name defect (not fully closed by that fix) or an unrelated
JIT-only defect in the same code path was not established in this session —
worth checking with the same instrumentation that page used
(`CRATONVM_DBG_JITC=1`, `try_jit_compile_callee`'s by-name resolution) before
assuming it's the identical mechanism.

## Not yet done

- No stack trace or log line names a Rust source location — this needs the
  same kind of instrumented rerun (`CRATONVM_DBG_JITC=1`, or a KRUN_STACK
  probe with the JIT's own diagnostics enabled) that closed the sibling bug.
- Not checked against G1/Generational specifically for this exact class (the
  full-suite run confirms it's collector-independent since it appeared
  identically on all three arms' FAIL lists, but the standalone isolation
  here was only run on ZGC).
- `TestClassScannerTests` and `HttpServiceProxyRegistrationAotProcessorTests`
  were not individually confirmed as members of this cluster.

## Reproducing

```bash
cd apps/spring-suite-runner
JDK25=<jdk25> CRATONVM_BIN=<cratonvm> ./run-suite.sh run \
  --only 'TestCompilerTests$' --tag repro          # fails: found=22 succ=3 fail=19
JDK25=<jdk25> CRATONVM_BIN=<cratonvm> ./run-suite.sh run \
  --jit off --only 'TestCompilerTests$' --tag repro-nojit   # passes: found=22 succ=22 fail=0
JDK25=<jdk25> ./run-suite.sh hotspot --only 'TestCompilerTests$' --tag repro-hotspot  # passes: found=22 succ=22 fail=0
```

## Related

- `aot-cglib-dynamicclassfileobject-illegalargumentexception-20260811-FIXED.md`
  — the architecturally-adjacent, already-fixed sibling.
