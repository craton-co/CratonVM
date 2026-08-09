# Retired 2026-08-05: the "Class-valued annotation element resolves to null" cluster was TWO causes

Branch `fix/annotation-class-element-null-20260805`. Retires
`docs/known-issues/springboot/annotation-class-element-resolves-null-cluster-20260805.md`.

That page grouped three frameworks — Spring's class-file metadata reader,
JUnit's `AnnotationUtils`, Byte Buddy's parameter binder — under one heading,
reasoning that "given the shared shape across three independent,
non-cooperating reflection front-ends, this points at one lower-level CratonVM
primitive shared by all three". The shared shape was real. The single primitive
was not.

**Cause 1** is the one the page names, and it is a genuine defect: an
annotation with a null `annotationType()` could escape into user code. Fixed
here, with a HotSpot-diffed regression test.

**Cause 2** is why the page's three named classes were failing, and it is not
an annotation bug at all: `383e7f5cf`, the JIT site-cache aliasing fix, which
landed on `dev` **after** the full-suite run this page was written from.

---

## Cause 1 — a nested annotation whose type is unresolvable escaped with a null `annotationType()`

The JDK's `sun.reflect.annotation.AnnotationParser` OMITS any annotation whose
type class is not resolvable rather than surfacing a broken one. A June fix
(`3ccd0defb`) taught the top-level array builders to do the same, for exactly
this reason — its commit message already names JUnit's
`findRepeatableAnnotations` NPEing on `annotationType().equals(...)`. Two holes
were left open:

1. **The nested-annotation element path never consulted that filter at all.**
   `annotation_element_to_java_typed`'s `Annotation` / `Array` arms built
   proxies directly. A `@Repeatable` container whose CONTAINED annotation type
   is absent from the runtime classpath — the `org.apiguardian.api.API` shape,
   where a jar is compile-scoped only — therefore handed callers an array of
   live annotations with `annotationType() == null`. That is precisely the
   array JUnit's `findRepeatableAnnotations` recurses over, calling
   `candidateAnnotationType.equals(containerType)` on every entry: the page's
   Variant 2.

2. **The filter and the builder ran two DIFFERENT lookups**, so they could
   disagree. Only the filter retried `class_id_by_name_near` after
   `load_class`, and `class_id_by_name` is `find_unique_class_by_name`, which
   answers `None` for an AMBIGUOUS name. Once a second loader defines the same
   annotation type, the filter admits an annotation the builder cannot resolve
   — and the builder then published a proxy with a null type mirror AND, because
   the `AnnotationDefault` backfill is gated on that same ClassId, with none of
   its defaulted members filled in. That is the page's Variant 3 shape: Byte
   Buddy reads `@FieldValue.declaringType()`, an omitted `Class` member
   defaulting to `void.class`, and gets null.

Measured against HotSpot (`vm/tests/resources/annclassprobe/AcpGoneProbe.java`,
where `AcpGone.class` is moved to `compileonly/` after compilation so the
contained type is genuinely absent at runtime):

| | `AcpGoneTarget.getDeclaredAnnotations()` |
|---|---|
| HotSpot | throws `NoClassDefFoundError: [LAcpGone;` |
| CratonVM before | container returned; both `value()` entries have `annotationType() == null` |
| CratonVM after | `value()` throws `TypeNotPresentException`; nothing null-typed escapes |

HotSpot fails harder — `AnnotationType.<init>` resolves the container's own
`AcpGone[]` member descriptor eagerly, so `getDeclaredAnnotations()` itself
raises. CratonVM is allowed to be laxer and defer to member ACCESS, reusing the
`TypeNotPresentException` sentinel an unresolvable `Class`-valued member already
used. What it may NOT do is hand back a live annotation whose type is null.

The guarantee is now structural rather than conventional:
`resolve_annotation_type_class_id` is the single admission lookup,
`resolvable_annotations` threads the resolved `ClassId` through to
`create_annotation_proxy_with_type`, and the builder's loader-aware lookup
(needed for identity under classloader isolation) can only REFINE that answer,
never replace it with nothing.

Regression test: `vm/tests/annotation_class_element_null.rs`. RED pre-fix
(`GONE-SUMMARY: nullTypes=2`), GREEN post-fix (`nullTypes=0`).

### Two smaller defects found on the way

* **`Proxy$Dispatch.invokeProxy` routed members by reading the `Method`'s `name`
  FIELD.** `get_field_by_name` answers null for a name it cannot resolve, and an
  empty name fell through to the generic `handler.invoke(...)` tail — which asks
  an `AnnotationProxy` for a member literally called `invoke`, finds none, and
  returns **null for every annotation member regardless of declared type**.
  Falls back to `Method.getName()`. Also boxes a raw `AnnotationDefault`
  primitive on the way out: that door returns `Object`, and
  `coerce_value_for_return` maps a raw `Value::Int(0)` onto `Object(None)`, so an
  omitted `boolean … default false` member could read back as null.

* **`create_method_object`'s legacy flat slots could overwrite real JDK fields.**
  `METHOD_LEGACY_SLOT_*` is a flat 0..12 layout standing in for synthetic-JDK
  mode. The fallback fired on "the named writes did not read back", not on "this
  class has no named layout" — so on a REAL-layout `Method` those indices are
  not spare, they are other live fields. Now gated on
  `resolve_field_index_by_class_id(class_id, "clazz")`, on both the write side
  and the read side (a genuinely-null named field must stay null rather than
  pick up whatever unrelated field shares the legacy index).

  This was found while chasing cause 2 and initially mistaken for it. It is a
  correctness fix with **no measured behavioural delta** — see below.

## Cause 2 — the three named classes were failing on JIT site-cache aliasing

The page's three classes do not fail for cause 1. They pass serially on `dev`
and fail only under concurrency, and the annotation fix does not move the rate.

Three-arm interleaved A/B, `TracingAndMeterObservationHandlerGroupTests`,
Azure Linux, 2 lanes concurrent, n=24 per arm:

| arm | failures |
|---|---|
| `682f6ef64` (the branch's merge-base, unmodified) | 14 / 24 |
| + the cause-1 annotation fix only | 17 / 24 |
| + all four commits | 13 / 24 |

Indistinguishable. What the arms DO show is that the unmodified merge-base
fails ~57% of runs, while the binary the 08-05 full suite was cut from fails
2/32. Something in `dev` had made this class far worse than the page records.

Tracing the live failure produced a symptom with nothing to do with
annotations. Byte Buddy:

```
java.lang.IllegalStateException: Cannot access annotation property
  public abstract int net.bytebuddy.implementation.bind.annotation.Argument.value()
java.lang.IllegalArgumentException: public abstract int int.value() does not represent
  interface net.bytebuddy.implementation.bind.annotation.Argument
```

`getDeclaringClass()` on `Argument.value()` answered `int` — that method's
RETURN type. The same runs produced `Method.getName()` failing Byte Buddy's
bean-naming check (`getMockitoInterceptor() does not follow Java bean naming
conventions`), `getReturnType()` answering a non-`Class` (`Not a constant
annotation value: void`), `NoSuchMethodError: java.lang.String.isArray()Z`, and
a null `typeDescriptor`. That is one call site reading another's dispatch
metadata, not a corrupted annotation.

Anchors were measured before use, not assumed:

| binary | failures |
|---|---|
| `1078f6f05c` — the 08-05 full-suite point | 2 / 12 |
| `682f6ef64` — 14 hours later | 13 / 23 |

Ancestry closes it without finishing the bisect:

| commit | landed | role | `1078f6f05c` | `682f6ef64` | current `dev` |
|---|---|---|---|---|---|
| `836631dcc` *site-cache EVERY native from compiled code, not just the leaves* | 09:10 | amplifier | no | yes | yes |
| `383e7f5cf` *a recycled `JitInvokeInfo` address let one call site serve another's dispatch* | 13:22 | **fix** | no | no | **yes** |

Widening the site cache made `JitInvokeInfo` address recycling common; the
recycled address let one call site serve another's dispatch.
`apps/spring-boot-suite-runner/RESULTS-20260805-jit-site-cache-aliasing-verify.md`
records five other classes going green on `383e7f5cf` alone, with the same
`java.lang.classfile`-shaped corruption.

The page was written from a full-suite run whose binary was cut hours before
that fix landed. That is the whole reason three independent frameworks looked
like one annotation cluster: they were all reading reflection metadata through
a dispatch cache that could hand back another site's entry.

## All three classes, after both causes are in the tree

Windows box, one process per class, `-Parallel 1`, JIT on. The "before" column
is this same harness on the branch's merge-base (no `383e7f5cf`), measured
earlier the same day:

| Class | before | after |
|---|---|---|
| `TracingAndMeterObservationHandlerGroupTests` | PASS 4/4 | **PASS 4/4** |
| `RestClientAutoConfigurationTests` | FAIL 14/15 | **PASS 15/15** |
| `ReactiveOAuth2ResourceServerAutoConfigurationTests` | CRASH (0xC0000005) | **PASS 50/50** |

`RestClientAutoConfigurationTests`'s before-arm is worth noting: on this box it
failed with `AnnotatedTypeMetadata.getAnnotations()` returning **null** — a
different null site from any the page records, and one that also clears with the
aliasing fix. Reflection metadata read through an aliased dispatch cache can
come back null, wrong-typed, or wrong-valued; which of those you see is an
accident of what the recycled entry happened to point at. That is why this
cluster looked like it had one shape and several unrelated call sites.

## What this leaves

Nothing open on cause 1. Cause 2 is fixed upstream and verified by its own
page. The lesson worth keeping is the page's own reasoning step — *three
independent front-ends showing one shape must share a primitive* — which was
sound, but the shared primitive was the JIT's dispatch cache, one layer below
where the page went looking.

## Reproduce

```bash
# cause 1, deterministic, no fixture needed
cargo test --release -p cratonvm-vm --test annotation_class_element_null
```

```bash
# cause 2, on Azure Linux (needs concurrency; serial runs pass either way)
/data/sbrun.sh <exe> jit module/spring-boot-micrometer-tracing \
  org.springframework.boot.micrometer.tracing.autoconfigure.TracingAndMeterObservationHandlerGroupTests \
  /tmp/out 12 900
```
