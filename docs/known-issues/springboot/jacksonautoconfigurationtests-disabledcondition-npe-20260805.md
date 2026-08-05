# `JacksonAutoConfigurationTests` — JUnit5 `DisabledCondition` NPE, 2026-08-05

**Status: OPEN — found 2026-08-05**

## Symptom

`module/spring-boot-jackson`'s `JacksonAutoConfigurationTests` shows
`tests=69 failed=22 containersFailed=32`. This is a **different bug** from
`docs/internal/fixed-suite-bugs/springboot/jacksonautoconfigurationtests-severe-slowdown-FIXED-20260722.md`,
which fixed a throughput problem (`update_root_snapshot`'s O(stack-depth)
per-native-call rebuild) that made the class exceed its timeout —
confirmed there via 162/162 passing at ~225s. Today's failure is not a
timeout at all; it is a correctness exception thrown for a large fraction of
the class's test methods:

```
java.lang.NullPointerException: Cannot invoke "java.util.Optional.isPresent()"
  because the return value of
  "org.junit.platform.commons.util.AnnotationUtils.findAnnotation(java.lang.reflect.AnnotatedElement, java.lang.Class)"
  is null
```

and, more specifically, inside JUnit Jupiter's own `@Disabled` condition
evaluator:

```
org.junit.jupiter.engine.execution.ConditionEvaluationException: Failed to
  evaluate condition [org.junit.jupiter.engine.extension.DisabledCondition]:
  Cannot invoke "java.util.Optional.isPresent()" because "metaAnnotation" is null
  ... ConditionEvaluator.evaluationException
Caused by: java.lang.NullPointerException: Cannot invoke
  "java.util.Optional.isPresent()" because "metaAnnotation" is null
```

`org.junit.platform.commons.util.AnnotationUtils.findAnnotation` is declared
to return `Optional<A>` (never a raw `null`) for exactly this reason —
JUnit5's own `DisabledCondition`/meta-annotation lookup code calls
`.isPresent()` on its result unconditionally. Getting a bare `null` back
instead of an empty `Optional` means CratonVM's reflection layer is
returning `null` from whatever native path backs this annotation lookup,
rather than the JDK's own (pure-Java, not natively-implemented)
`AnnotationUtils` logic malfunctioning.

## Root cause

**Not confirmed — needs further investigation.** `AnnotationUtils.findAnnotation`
recurses through `AnnotatedElement.getDeclaredAnnotations()`/
`getAnnotations()` and `findMetaAnnotation`, calling back into
`Class`/`Method`/`Parameter` reflection repeatedly. Since JUnit's own method
body is ordinary Java bytecode (not a CratonVM native), a `null` where an
`Optional` is contractually required most likely comes from one of the
underlying reflective annotation-lookup natives
(`native-builtins/src/lang_class.rs`'s annotation-proxy machinery, already
the confirmed source of at least one related bug — see
`jacksonautoconfigurationtests-severe-slowdown-FIXED-20260722.md`'s
"caches method/constructor annotation proxies" fix) returning a raw `null`
array/reference for some `AnnotatedElement` shape specific to this class's
`@ParameterizedTest`/`MapperType`-parameterized methods, rather than an
empty result. Given this is JUnit5 engine machinery (not Jackson-specific
code), it plausibly affects any class whose `@Disabled`/meta-annotation
scanning hits the same reflective shape — worth a targeted matrix probe
against HotSpot per the project's usual method for this class of bug.

## Affected classes

- `module/spring-boot-jackson` — `org.springframework.boot.jackson.autoconfigure.JacksonAutoConfigurationTests`

Log: `craton-fullsuite-azure-20260805-s5/all-jit/logs/module_spring-boot-jackson.org.springframework.boot.jackson.autoconfigure.JacksonAutoConfigurationTests.{out,err}.log`
