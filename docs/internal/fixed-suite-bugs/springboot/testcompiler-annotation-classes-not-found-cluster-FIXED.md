# Spring's in-memory `TestCompiler` annotation/platform listing and generated-resource failures — FIXED

**Status: FIXED 2026-07-13. Severity: MEDIUM.**

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

The initial theory was an incomplete `java.base` exposure. The 2026-07-12
investigation below ruled out the raw JRT filesystem itself and narrowed the
failure to CratonVM's forced-native `JavacFileManager.list` implementation.
The completed investigation found a second residual after compilation was
unblocked: generated annotation-processor resources were exposed through a
handler-backed `resource:` URL, but CratonVM's native `URL.openStream` ignored
that handler and searched only its static classpath index.

## 2026-07-12 Azure investigation — historical narrowing

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

That evidence correctly moved the target from JRT parsing to javac's file
manager listing/completion path. It was not sufficient by itself to retire the
issue because direct lookup and package enumeration exercise different paths.

## Root causes and fixes

Four runtime defects and moving-GC residuals were required to close the
seven-class cluster completely:

1. CratonVM force-dispatches `JavacFileManager.list`. The original native used
   a small platform-class allowlist; the later full-JRT implementation still
   retained native arguments and hundreds of returned `JavaFileObject`s in
   GC-unsafe raw references/native pins. A young collection during
   `getJavaFileForInput` could truncate or corrupt the package inventory before
   javac completed symbols. The fixed implementation pins and refreshes every
   Java argument, honors the requested `JavaFileObject.Kind`, and streams the
   full jimage package inventory directly into a pinned exact-capacity Java
   `ArrayList` backing array. This avoids both stale moving-GC references and
   native-root-window exhaustion.
2. Once compilation and annotation processing succeeded, the metadata tests
   failed while reading `../../../../apps/META-INF/spring-configuration-metadata.json`.
   Spring's `DynamicClassLoader` creates a `resource:` URL with an
   application-provided `URLStreamHandler` whose connection owns the generated
   in-memory bytes. CratonVM's native `URL.openStream` treated every
   `resource:` URL as a VM-synthesized static classpath resource. It now
   delegates to a non-JDK custom handler first when one is present, while URLs
   synthesized by CratonVM (which have no handler) retain the previous static
   lookup behavior.
3. A current-`dev` rerun then exposed another moving-GC window in native
   `Stream.collect`: materializing the lazy stream could move the tagged
   `Collector` before CratonVM read its tag. A `Collectors.toSet()` collector
   could therefore be mistaken for an `ArrayList` collector. The collector is
   now pinned across stream materialization and refreshed before its tag and
   fields are read.
4. After the collector itself was protected, the materialized stream elements
   were still raw references while the synthetic `HashSet`, backing map, and
   buckets were allocated and populated. A `TypeElement` could move and the
   processor would later observe an unrelated `java.lang.Object`. `make_set_of`
   now pins every input element and each partially-built collection object,
   refreshes them before use, and releases the complete pin window on both
   success and error.

## Final verification (Azure, JDK 25, 2026-07-13)

Unique final binary:

```text
/data/victor-worktrees/cratonvm-spring-testcompiler-annotation-complete-20260713-10
sha256 f14d72b9b341f9d27db06c1dea1ebf31b3b071c2a494ec1bf86f7e4e0a239062
```

Focused runtime contracts all pass:

- `JavacPlatformProbe`: `Deprecated`, `ElementType`, and `RetentionPolicy`
  resolve as `TypeElement`s; direct `java.lang.annotation` listing returns all
  12 class files; a `Kind.OTHER` listing returns zero.
- `AnnotationCompileOnlyProbe`: annotated in-memory compilation returns
  `compile=true`.
- `ProcessorProbe`: the explicitly installed processor receives both normal
  and processing-over rounds and compilation returns true.
- Focused Rust guards pass:
  `jrtfs_javac_listing_tests::javac_platform_listing_uses_complete_jrt_package_inventory`
  and
  `runtime::interpreter::tests::standard_location_force_native_covers_javac_regex_shortcut`.
- The complete `cratonvm-native-collections` library suite passes (72 tests),
  and three fresh `MethodBasedMetadataGenerationTests` VM processes pass
  10/10 each (30/30 total), directly stressing the shifted collector/element
  residuals.

The clean Spring Boot 4.1.1-SNAPSHOT rerun used the module's Gradle-generated
test runtime classpath, one VM process per class, and a 1500-second per-class
ceiling. All seven target classes pass: **94 tests started, 94 successful, 0
failed, 0 aborted**. Logs are under:

```text
/data/victor-worktrees/testcompiler-annotation-suite-20260713-05-final/
```

The three-run focused stress logs are under
`/data/victor-worktrees/testcompiler-annotation-elementpin-stress-20260713-10-final/`.

## Historical repro

```powershell
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -ClassList <TSV row for configuration-metadata/spring-boot-configuration-processor's PropertyDescriptorResolverTests> `
  -Start 1 -Count 1 -TimeoutSec 300 -Exe <cratonvm exe>
```
Standalone repro should be simpler: any program calling
`ToolProvider.getSystemJavaCompiler().getTask(...)` to compile a source
string referencing `java.lang.annotation.ElementType` under CratonVM.
