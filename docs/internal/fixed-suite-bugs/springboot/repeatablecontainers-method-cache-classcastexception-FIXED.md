# `RepeatableContainers$StandardRepeatableContainers` method-cache `ClassCastException` — NO LONGER REPRODUCES

**Status: CLOSED 2026-07-20 — does not reproduce on current `dev`. Root cause was never
pinned; treat as resolved by unrelated intervening work, not a confirmed fix.**

## Original symptom (2026-07-17)

```
JUnit Jupiter:ChildManagementContextInitializerAotTests:aotContributedInitializerStartsManagementContext(CapturedOutput)
  => org.springframework.beans.factory.BeanDefinitionStoreException: Failed to parse configuration class [org.springframework.boot.actuate.autoconfigure.endpoint.web.WebEndpointAutoConfiguration]
     org.springframework.context.annotation.ConfigurationClassParser.parse(ConfigurationClassParser.java:196)
     ...
   Caused by: java.lang.ClassCastException: java.lang.Object cannot be cast to java.lang.reflect.Method
     org.springframework.core.annotation.RepeatableContainers$StandardRepeatableContainers.getRepeatedAnnotationsMethod(RepeatableContainers.java:266)
     org.springframework.core.annotation.RepeatableContainers$StandardRepeatableContainers.findRepeatedAnnotations(RepeatableContainers.java:256)
     org.springframework.core.annotation.AnnotationTypeMappings.addMetaAnnotationsToQueue(AnnotationTypeMappings.java:97)
     org.springframework.core.annotation.AnnotationTypeMappings.<init>(AnnotationTypeMappings.java:74)
     org.springframework.core.annotation.AnnotationTypeMappings$Cache.createMappings(AnnotationTypeMappings.java:290)
     org.springframework.core.annotation.TypeMappedAnnotation.of(TypeMappedAnnotation.java:615)
     org.springframework.core.type.classreading.ClassFileAnnotationDelegate.createMergedAnnotation(ClassFileAnnotationDelegate.java:80)
     org.springframework.core.type.classreading.ClassFileMetadataReader.<init>(ClassFileMetadataReader.java:45)
     org.springframework.context.annotation.ConfigurationClassParser.retrieveBeanMethodMetadata(ConfigurationClassParser.java:466)
```

The original hypothesis (unconfirmed) was that CratonVM's `Map`/
`ConcurrentReferenceHashMap` handling returned a value from the wrong key, or
that the `NONE` sentinel's identity was unstable, causing
`getRepeatedAnnotationsMethod`'s `(Method) result` cast to fail. Full
analysis in git history at the retired doc's previous revision
(`docs/known-issues/springboot/repeatablecontainers-method-cache-classcastexception.md`).

## 2026-07-20 investigation — root cause never pinned, but no longer reproduces

Working the doc's assigned closure task in worktree
`CratonVM-repeatablecontainers-cache-20260720` (branch
`fix/repeatablecontainers-method-cache-20260720`), built a fresh binary from
current `dev` (`d999dc76f`) and re-ran the exact affected class,
`ChildManagementContextInitializerAotTests`, four times end-to-end via the
class's own JUnit Platform launch (real spring-boot-actuator-autoconfigure
module classpath, `--java-home` real JDK 25 boot).

**The original `ClassCastException` did not reproduce in any of the four
runs.** `RepeatableContainers`/`AnnotationTypeMappings`/
`ClassFileAnnotationDelegate` are never mentioned in any of the four
captured stack traces. AOT config-class parsing (the phase where the
original bug lived) completes successfully every time — the test progresses
substantially further than the original failure point (through
`ConfigurationClassParser`, bean definition registration, and into actually
starting a mock servlet web server), before failing for a **completely
different, unrelated reason**: see
[`mockresolver-dynamicclassloader-classnotfound-forked-testcontext.md`](../../known-issues/springboot/mockresolver-dynamicclassloader-classnotfound-forked-testcontext.md).

Also independently probed the underlying mechanism directly: `RepeatableContainers`'s
cache field is a real `org.springframework.util.ConcurrentReferenceHashMap<Class<?>, Object>`
(confirmed via `javap` on the real `spring-core-7.0.7.jar` classes — not any
CratonVM synthetic/native-substituted collection). A standalone probe
(`CrhmRepro.java`, driving a real `ConcurrentReferenceHashMap` through
`computeIfAbsent` with thousands of distinct `Class` keys, concurrently,
across many rounds, with `System.gc()` between rounds to stress the
segment-restructuring/reference-purge machinery) found **zero** identity or
type mismatches on current `dev` (120,000+ ops, 0 mismatches, 0
`ClassCastException`s) — consistent with the doc's symptom no longer being
reproducible, though this does not by itself prove the original hypothheses
were wrong, only that the specific failure mode is gone.

**Conclusion: closing as no-longer-reproducing.** The most likely explanation
is that one of the many collection/GC/moving-young-gen fixes landed on `dev`
between 2026-07-17 and 2026-07-20 (see `docs/known-issues/springboot/README.md`'s
closure history for that window) incidentally fixed whatever underlying
`Map`/reference-handling defect caused this. No specific commit was
identified as *the* fix — if this ever regresses, re-open with a fresh
`git bisect` between those dates using the `CrhmRepro.java` probe (kept
under `../../../known-issues/repros/springboot/` — see below) as a fast,
Spring-independent repro.

## Repro tooling kept for future use

`CrhmRepro.java` (standalone, needs only `spring-core-7.0.7.jar` on the
classpath, no Spring Boot checkout) drives a real
`ConcurrentReferenceHashMap<Class<?>, Object>` through `computeIfAbsent`
exactly like `RepeatableContainers$StandardRepeatableContainers`, with
configurable key count / rounds / thread count. Useful for any future
`ConcurrentReferenceHashMap`-adjacent regression report. See
`docs/known-issues/repros/repeatablecontainers-crhm/README.md`.

## Affected classes (original)

| Module | Class |
|---|---|
| `module/spring-boot-actuator-autoconfigure` | `org.springframework.boot.actuate.autoconfigure.web.server.ChildManagementContextInitializerAotTests` |
