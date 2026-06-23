# HIB-CV-25 — CDI/Weld broken: meta-annotation reflection misclassifies qualifiers (`WELD-001301`)

**Run:** full Hibernate ORM suite, 2026-06-22/23
**Binary:** `cvhibtest.exe` (dev `c863b23e`)
**Severity:** High — whole CDI integration broken; **deterministic, reproduces under `--nojit`**, HotSpot PASS
**Status:** Confirmed; root area = annotation / meta-annotation reflection

---

## Symptom

The `org.hibernate.orm.test.cdi.*` family fails to bootstrap CDI (Weld). Two
related signatures, both with HotSpot **PASS**:

```
# most cdi.* tests:
org.jboss.weld.exceptions.IllegalArgumentException: WELD-001301: Annotation
QualifierInstance {annotationClass=interface jakarta.enterprise.inject.Produces, values={}}
is not a qualifier

# the cdi.*.delayed variants:
java.lang.RuntimeException: Could not configure StandardServiceRegistryBuilder
```

Affected (HotSpot PASS, CratonVM `--nojit` FAIL) — non-exhaustive:
`CdiSmokeTests`, `SimpleTests`, `StandardCdiSupportTest`, `ValidExtendedCdiSupportTest`,
`HibernateSearchStandardCdiSupportTest`, `HibernateSearchDelayedCdiSupportTest`,
`DelayedMixedAccessTest`, `ExtendedMixedAccessTest`, `ImmediateMixedAccessTests`,
`DelayedCdiHostedConverterTest`, `DelayedCdiSupportTest`.

## Root cause area

`WELD-001301` is thrown when Weld is handed an annotation it treats as a CDI
**qualifier** but which is **not** annotated `@jakarta.inject.Qualifier`. Here the
offending annotation is **`@jakarta.enterprise.inject.Produces`** — which is *not*
a qualifier. So CratonVM caused Weld to build a `QualifierInstance` for `@Produces`
and then its own validation rejected it.

This means CratonVM's **annotation / meta-annotation reflection is wrong**: Weld
classifies an annotation as a qualifier via
`annotationType.isAnnotationPresent(jakarta.inject.Qualifier.class)` /
`getAnnotations()` on the *annotation type itself* (a meta-annotation lookup). The
misclassification of `@Produces` indicates CratonVM returns incorrect
meta-annotation data for annotation types (e.g. wrong/empty/over-broad results
from `Class.getAnnotations()` / `isAnnotationPresent` on an annotation interface),
breaking Weld's qualifier discovery and thus the whole CDI bootstrap.

(The `Could not configure StandardServiceRegistryBuilder` variant is the same
failure surfacing earlier during the delayed-CDI bean-manager setup.)

## Why it's a real CratonVM bug

- Deterministic, reproduces standalone under `--nojit` (not the JIT family).
- HotSpot PASS on every listed class.

## Reproduce

```
cvhibtest.exe --java-home <jdk25> --nojit @common.args -Dcraton.trace=1 \
  CratonRunner <list-with-org.hibernate.orm.test.cdi.type.CdiSmokeTests> 0
# -> WELD-001301: ... @Produces ... is not a qualifier ; 0 ok / 1 failed
```

## Suggested next step for a fixer

Minimal repro: reflect over an annotation **type** (e.g. `Produces.class` and a
real qualifier like a `@Qualifier`-annotated annotation) and compare
`getAnnotations()` / `getDeclaredAnnotations()` / `isAnnotationPresent(Qualifier.class)`
output to HotSpot. The defect is almost certainly in how CratonVM materializes
meta-annotations on annotation interfaces.

## Triage

Real, high-impact (all CDI usage), deterministic. Independent of the JIT. Strong
**hand-off** candidate for whoever owns annotation reflection / metadata.
