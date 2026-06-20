# JUnit 5 `@ExtendWith` meta-annotation not applied → `No ParameterResolver registered`

**Status:** 🔴 **OPEN** (found 2026-06-20). Annotation-synthesis family — **not** a JTA/JAXB/socket bug.
**Severity:** High for running real-world JUnit 5 suites (Hibernate, Spring, …) on CratonVM: it blocks any
test whose extension is contributed by a **composed/meta annotation**.

## Symptom

Running a Hibernate ORM 8.1 test class via the JUnit Platform launcher on CratonVM:

```
@@FAIL success(EntityManagerFactoryScope) :
  org.junit.jupiter.api.extension.ParameterResolutionException:
  No ParameterResolver registered for parameter [...EntityManagerFactoryScope arg0]
  in method [...JtaCustomAfterCompletionTest.success(...EntityManagerFactoryScope)].
```

HotSpot runs the same class fine. Tests are *discovered* and *started* (so launcher + engine work), but the
class-level extensions are never registered, so the `EntityManagerFactoryScope` parameter can't be resolved
and the EntityManagerFactory is never bootstrapped.

## Root cause (CratonVM reflection / annotation synthesis)

The test class is annotated with `@Jpa`, which is a **composed annotation** carrying
`@ExtendWith(EntityManagerFactoryExtension.class)` + `@ExtendWith(EntityManagerFactoryParameterResolver.class)`
as **meta-annotations** (and `@ExtendWith` is itself `@Repeatable(Extensions.class)`):

```java
@Inherited @Retention(RUNTIME) @Target({TYPE, METHOD})
@TestInstance(PER_CLASS)
@ExtendWith(EntityManagerFactoryExtension.class)
@ExtendWith(EntityManagerFactoryParameterResolver.class)
@ExtendWith(FailureExpectedExtension.class)
public @interface Jpa { ... }
```

JUnit's `AnnotationUtils.findRepeatableAnnotations(testClass, ExtendWith.class)` recursively walks
meta-annotations (`testClass → @Jpa → (meta) @Extensions/@ExtendWith`). On CratonVM that recursive
meta-present repeatable lookup yields **nothing**, so JUnit registers zero extensions for the class.

Note: the direct-element `@Repeatable` merge (`getAnnotationsByType` direct + container) **is** on `dev`
(`0749b0dd`, bug06-fam6). This gap is the **meta-annotation** arm — `@ExtendWith` repeated on the annotation
*type* `@Jpa`, surfaced through the annotation type's own `getDeclaredAnnotations()` / the `@Extensions`
container — which is still not surfaced the way `findRepeatableAnnotations` expects. Same family as the
open `bug06-fam6` annotation-synthesis cluster; see [[bug06-fam6-annotation-synthesis-progress]].

## Impact

Blocks running the real Hibernate ORM JUnit suite end-to-end on CratonVM (every `@Jpa` / `@SessionFactory` /
`@DomainModel` test resolves its scope parameter from a meta-`@ExtendWith` extension). The underlying
subsystems those tests exercise can still be validated by **programmatic bootstrap** (see
`_hibrepro/HibBoot`, `HibJoined`), which is how the Hibernate JTA + JAXB/ByteBuddy paths were verified
without the launcher.

## Repro

`_hibrepro/Run.java` (minimal `LauncherFactory` + `selectClass`) on
`org.hibernate.orm.test.actionqueue.JtaCustomAfterCompletionTest` →
`@@SUMMARY ... failed=2` with the `ParameterResolutionException` above. A pure-JDK repro: a custom composed
annotation `@Meta` carrying `@ExtendWith(MyResolver.class)`, applied to a test taking the resolved parameter.
