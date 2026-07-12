# Spring's in-memory `TestCompiler` can't resolve `java.lang.annotation.*` — 7 classes in `spring-boot-configuration-processor`

**Status: OPEN, characterized. Severity: MEDIUM (narrow module, but blocks
all annotation-processor testing there).**

Found while triaging the HANG-rerun of the first Spring Boot suite run (see
[[project_spring_boot_suite_runner_20260711]]) — 7 classes in
`configuration-metadata/spring-boot-configuration-processor`
(`GenericsMetadataGenerationTests`, `LombokMetadataGenerationTests`,
`JavaBeanPropertyDescriptorTests`, `MergeMetadataGenerationTests`,
`LombokPropertyDescriptorTests`, `MethodBasedMetadataGenerationTests`,
`PropertyDescriptorResolverTests`) — originally hit the 300s timeout
(consistent with in-memory compilation being much slower under CratonVM),
and at 1500s complete but FAIL with:

```
org.springframework.core.test.tools.CompilationException: Unable to compile source

Errors:
- cannot find symbol
  symbol: class Deprecated /org/springframework/boot/configurationsample/method/DeprecatedClassMethodConfig.java 27:2

Warnings:
- unknown enum constant java.lang.annotation.ElementType.TYPE
  reason: class file for java.lang.annotation.ElementType not found
- unknown enum constant java.lang.annotation.ElementType.METHOD
- unknown enum constant java.lang.annotation.RetentionPolicy.RUNTIME
  reason: class file for java.lang.annotation.RetentionPolicy not found
```

## Analysis

Spring's `org.springframework.core.test.tools.TestCompiler` compiles small
Java source fixtures **in-memory** via the real `javax.tools.JavaCompiler`
(`ToolProvider.getSystemJavaCompiler()`), used here to feed
`AnnotationMetadataGenerationTests`-style fixtures into the real
`configuration-processor` annotation processor and assert on the generated
`spring-configuration-metadata.json`. The compiler itself runs, but can't
resolve even foundational `java.lang.annotation.*` types
(`Deprecated`, `ElementType`, `RetentionPolicy`) — i.e. **the platform
classpath/bootclasspath `javac` is compiling against is incomplete or not
being passed through**, not a fixture-source problem (the fixture source
files themselves are real, unmodified Spring Boot test resources on the
module's own classpath, already verified present).

The initial theory was an incomplete `java.base` exposure.  The 2026-07-12
investigation below rules out the raw JRT filesystem, module enumeration, and
file-manager lookup as the direct cause.  The remaining fault is in javac's
in-process platform-symbol completion: readable JRT class files for these
types do not become `TypeElement`s.

## 2026-07-12 Azure investigation — narrowed, still open

Reproduced from current `dev` with an isolated CratonVM build and the same
standalone in-memory compiler probe on JDK 17, 21, and 25.  On HotSpot,
`ToolProvider.getSystemJavaCompiler()` returns `com.sun.tools.javac.api.JavacTool`
and compiles `@Deprecated class AnnotationCompileProbe {}`.  Under CratonVM it
returns that same compiler implementation, compiles an unannotated class, and
then fails the annotated source with `cannot find symbol: class Deprecated`.
Changing Java homes therefore does not alter the failure.

The runtime can now expose the relevant JRT data to the standard file manager:

* `jrt:/modules/java.base/java/lang/Deprecated.class` exists and reads as the
  expected class file (647 bytes, `CAFEBABE`).
* `StandardJavaFileManager.getJavaFileForInput(PLATFORM_CLASS_PATH,
  "java.lang.Deprecated", CLASS)` returns a readable `JRTFileObject` with the
  correct binary name.
* The JRT index enumerates all 69 system modules and contains both
  `java/lang/Deprecated.class` and all 12 files in
  `java/lang/annotation`; `/packages/java.lang.annotation/java.base` is a
  symbolic link to `/modules/java.base`.

The decisive differential is before compilation analysis: on CratonVM,
`JavacTask.getElements().getTypeElement("java.lang.Object")` and `String`
resolve, but `getTypeElement("java.lang.Deprecated")` and
`getTypeElement("java.lang.annotation.ElementType")` are `null`.  All four
resolve on HotSpot.  Disabling javac's symbol file and using an explicit
standard file manager do not change that result.

Therefore this is not closed by a successful `Files`/`JavaFileManager` JRT
probe.  It is a broader in-process javac platform-symbol completion residual,
with annotation types as the first user-visible cluster.  The next debugging
target is javac's `ClassFinder`/`JavacFileManager` path: determine why valid,
readable platform `JRTFileObject`s outside the bootstrap subset fail to create
symbols.

## Repro

```powershell
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -ClassList <TSV row for configuration-metadata/spring-boot-configuration-processor's PropertyDescriptorResolverTests> `
  -Start 1 -Count 1 -TimeoutSec 300 -Exe <cratonvm exe>
```
Standalone repro should be simpler: any program calling
`ToolProvider.getSystemJavaCompiler().getTask(...)` to compile a source
string referencing `java.lang.annotation.ElementType` under CratonVM.
