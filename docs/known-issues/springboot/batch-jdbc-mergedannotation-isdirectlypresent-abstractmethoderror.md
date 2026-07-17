# `MergedAnnotation.isDirectlyPresent()` — `AbstractMethodError` while importing `HibernateJpaAutoConfiguration`

**Status: OPEN — found 2026-07-17, root cause not pinned to a file:line**

## Symptom

Module `module/spring-boot-batch-jdbc`, class `BatchJdbcAutoConfigurationTests`
— 2 of its 26 failures (`testUsingJpa`, `testRegisteredAndLocalJob`) share
this shape:

```
Caused by: org.springframework.beans.factory.BeanDefinitionStoreException: Failed to process import candidates for configuration class [org.springframework.boot.hibernate.autoconfigure.HibernateJpaAutoConfiguration]: method org/springframework/core/annotation/MergedAnnotation.isDirectlyPresent()Z has no Code attribute
```

and, in the other occurrence, the same underlying exception surfaces
unwrapped:

```
Caused by: java.lang.AbstractMethodError: method org/springframework/core/annotation/MergedAnnotation.isDirectlyPresent()Z has no Code attribute
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-batch-jdbc.org.springframework.boot.batch.jdbc.autoconfigure.BatchJdbcAutoC-adef578bf5b8.out.log`
(lines ~3701, ~3726 — the file is 860KB; the other 24 failures in this class
are the unrelated `dataSource`/`shutdown` destroy-method-ambiguity cluster,
see [`jooq-destroy-method-ambiguity-and-hang.md`](jooq-destroy-method-ambiguity-and-hang.md)).

## Root cause

**Not confirmed.** `MergedAnnotation.isDirectlyPresent()Z` is Spring's own
interface (`org.springframework.core.annotation.MergedAnnotation`), not a
JDK type — real Spring bytecode (`TypeMappedAnnotation` and friends) always
implements every `MergedAnnotation` method with real, present bytecode, so
an `AbstractMethodError` with "no Code attribute" for a *Spring-authored*
interface method is a different flavor of symptom than the sibling
JDK-interface cases in this same rerun
([`hateoas-stream-reduce-triarg-missing-native-abstractmethoderror.md`](hateoas-stream-reduce-triarg-missing-native-abstractmethoderror.md),
[`integration-mbeanserver-getdomains-missing-native-abstractmethoderror.md`](integration-mbeanserver-getdomains-missing-native-abstractmethoderror.md)),
where the shape is "a CratonVM-allocated synthetic object stamped with a
bare JDK interface, one method short of its native registration set."

Grepped `native-builtins/src/*.rs` and `native-collections/src/lib.rs` for
`MergedAnnotation`: no native registration or synthetic-allocation site for
this interface exists anywhere in the codebase (all hits are unrelated
comments referencing `MergedAnnotation`/`MergedAnnotations` in the context
of other bugs, e.g. bridge-method handling in `lang_class.rs` and a
`MergedAnnotations.stream()`-related `native-collections` fast path). That
rules out the "synthetic object typed as the bare interface" explanation
that fits the other two `AbstractMethodError` docs in this batch — this
receiver is presumably real Spring bytecode (a real `TypeMappedAnnotation`
or similar). The likelier mechanisms, neither verified:

1. A real annotation-metadata object gets constructed with a **stale or
   substituted `ClassId`/vtable** that doesn't match its true runtime class
   (i.e. an object-identity/class-tagging bug elsewhere in CratonVM's
   annotation-introspection machinery causes dispatch to resolve against
   the wrong, abstract-only class), or
2. `Class.forName`/class-loading for the real `TypeMappedAnnotation`
   implementation is itself partially failing during
   `HibernateJpaAutoConfiguration`'s `@Import` candidate processing (this
   error only fires while Spring's `ConfigurationClassParser` is walking
   `@Conditional`/`@Import` annotations on that specific autoconfiguration
   class — narrow enough that it might be tied to something specific about
   that class's annotation shape rather than a general
   `MergedAnnotation` gap).

**Next step for whoever picks this up:** a standalone repro constructing a
`MergedAnnotation` over `HibernateJpaAutoConfiguration.class` directly (no
Spring context) and calling `isDirectlyPresent()` on it, with
`CRATONVM_DBG_OOBFIELD`/dispatch tracing enabled, would confirm whether the
receiver's actual runtime class is what it should be.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-batch-jdbc` | `org.springframework.boot.batch.jdbc.autoconfigure.BatchJdbcAutoConfigurationTests` (2 of 26 failing test methods: `testUsingJpa`, `testRegisteredAndLocalJob`) |
