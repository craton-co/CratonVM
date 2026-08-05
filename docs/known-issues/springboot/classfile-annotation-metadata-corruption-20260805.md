# Spring's `java.lang.classfile`-based annotation metadata reading is corrupted under CratonVM — `WebMvcAutoConfigurationTests`, `WebMvcObservationAutoConfigurationTests`, `ServletComponentScanIntegrationTests`

**Status: OPEN — found 2026-08-05**

## Symptom

Three classes in the webmvc/web-server bootstrap area fail heavily, all
inside Spring's annotation-metadata reading machinery
(`org.springframework.core.type.classreading.*` /
`org.springframework.core.type.AnnotatedTypeMetadata` /
`org.springframework.core.annotation.MergedAnnotations`), with three
distinct-looking but same-family symptoms:

1. `WebMvcAutoConfigurationTests` — **88/93 tests fail** with:
   ```
   java.lang.classfile.constantpool.ConstantPoolException: Bad CP index: 23296
     org.springframework.core.type.classreading.ClassFileAnnotationDelegate.createMergedAnnotations(ClassFileAnnotationDelegate.java:53)
     org.springframework.core.type.classreading.ClassFileMethodMetadata.of(ClassFileMethodMetadata.java:155)
     org.springframework.core.type.classreading.ClassFileAnnotationMetadata.lambda$of$0(ClassFileAnnotationMetadata.java:215)
     org.springframework.core.type.classreading.ClassFileMetadataReader.<init>(ClassFileMetadataReader.java:45)
   ```
   A second distinct bad index (`23041`) also appears in the same log.
   Both indices are implausibly large for these fixture classes' actual
   constant pools (normally low hundreds of entries) — consistent with the
   `java.lang.classfile` parser reading from the wrong buffer/offset
   entirely rather than a genuinely malformed but small class file.

2. `WebMvcObservationAutoConfigurationTests` — **12/13 tests fail** with:
   ```
   java.lang.NullPointerException: Cannot invoke "org.springframework.core.annotation.MergedAnnotation.isPresent()" because "annotation" is null
   ```

3. `ServletComponentScanIntegrationTests` — **2/3 tests fail** with:
   ```
   java.lang.NullPointerException: Cannot invoke "org.springframework.core.annotation.MergedAnnotations.stream()" because the return value of "org.springframework.core.type.AnnotatedTypeMetadata.getAnnotations()" is null
     org.springframework.context.annotation.AnnotationConfigUtils.attributesForRepeatable(AnnotationConfigUtils.java:308)
     org.springframework.context.annotation.ConfigurationClassParser.doProcessConfigurationClass(ConfigurationClassParser.java:313)
   ```

Possibly related: `WebTestClientAutoConfigurationTests` (3/11 failed,
`IllegalStateException: No ConfigurableListableBeanFactory set` when
asserting on a context whose refresh silently failed) may be a downstream
consequence of the same family — not confirmed, flagged for the next pass.

## Cross-check

HotSpot baseline (`hotspot-baseline-latest.tsv`) passes all three classes
cleanly: `WebMvcAutoConfigurationTests` 93/93, `WebMvcObservationAutoConfigurationTests`
13/13, `ServletComponentScanIntegrationTests` 3/3 (plus its sibling
`MockWebEnvironmentServletComponentScanIntegrationTests` 3/3). This is not
the CRLF-fixture confound (that affects text resources; these are binary
`.class` bytes and HotSpot's own classfile parsing of the identical
fixture succeeds). Genuinely CratonVM-specific.

No existing doc found for `ConstantPoolException`/`Bad CP index`,
`ClassFileMetadataReader`/`ClassFileAnnotationMetadata`, or this exact
`AnnotatedTypeMetadata.getAnnotations() is null` / `MergedAnnotation.isPresent()`
NPE shape (searched `docs/known-issues/` and `docs/internal/` by symptom
and by class name).

## Hypothesis (not yet confirmed)

Spring Framework 7 / Boot 4's classpath component scanner reads class
bytes for annotation inspection via `ClassLoader.getResourceAsStream(...)`
and hands them to the JDK's own `java.lang.classfile` API (new since
JDK 24) rather than ASM. All three failures sit downstream of that read:
a `ConstantPoolException` means the parser itself choked on the byte
stream; the two NPEs (`getAnnotations()` returning null,
`MergedAnnotation` being null mid-stream) are consistent with a *partial*
parse that CratonVM's `ClassFileMetadataReader` caller treats as "no
annotations" instead of propagating the earlier failure — i.e. these may
all trace to one upstream corruption in how CratonVM serves `.class`
bytes (length, offset, or buffering) to this specific reader path, not
three independent bugs. Needs targeted repro: a minimal
`getResourceAsStream` + `java.lang.classfile.ClassFile.parse(bytes)` probe
against one of the failing fixture classes, comparing byte-for-byte
against the on-disk `.class` file and against HotSpot's read of the same
resource.

## Affected classes

- `module/spring-boot-webmvc` — `org.springframework.boot.webmvc.autoconfigure.WebMvcAutoConfigurationTests`
- `module/spring-boot-webmvc` — `org.springframework.boot.webmvc.autoconfigure.WebMvcObservationAutoConfigurationTests`
- `module/spring-boot-web-server` — `org.springframework.boot.web.server.servlet.context.ServletComponentScanIntegrationTests`
- (possibly related, unconfirmed) `module/spring-boot-webtestclient` — `org.springframework.boot.webtestclient.autoconfigure.WebTestClientAutoConfigurationTests`
