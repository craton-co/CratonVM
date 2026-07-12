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

This points at a gap in how CratonVM wires
`ToolProvider.getSystemJavaCompiler()` / the module-path javac uses when
invoked reflectively from inside a running CratonVM process, as opposed to
being launched as its own `java.exe`/`javac.exe` process — likely missing or
incomplete `java.base` module exposure to the compiler's own classloader.
Not yet traced to a specific native/registration gap.

## Repro

```powershell
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -ClassList <TSV row for configuration-metadata/spring-boot-configuration-processor's PropertyDescriptorResolverTests> `
  -Start 1 -Count 1 -TimeoutSec 300 -Exe <cratonvm exe>
```
Standalone repro should be simpler: any program calling
`ToolProvider.getSystemJavaCompiler().getTask(...)` to compile a source
string referencing `java.lang.annotation.ElementType` under CratonVM.
