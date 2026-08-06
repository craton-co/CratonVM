# `getDeclaredAnnotations()` returned an element that was not an `Annotation`

**Status: FIXED 2026-08-06.** Found as the last CratonVM-only failure in the
1901-class mock sweep run while closing
[`threadlocal-retransform-drops-native-shadow-FIXED.md`](threadlocal-retransform-drops-native-shadow-FIXED.md),
and filed separately because it is a different defect in a different subsystem.

## Symptom

`Mockito.mock(org.infinispan.query.remote.client.impl.QueryRequest.class)` fails
on CratonVM and succeeds on HotSpot 25 with the identical classpath. The
`MockitoException` renders with an empty message, which is why this sat as "one
odd class" — the whole cause chain has to be printed to see anything:

```
MockitoException: Could not modify all classes [class java.lang.Object,
  interface …JsonSerialization, class …QueryRequest]
  <- IllegalStateException at InlineBytecodeGenerator.triggerRetransformation
  <- ClassCastException: jdk.proxy1.$Proxy42 cannot be cast to
       java.lang.annotation.Annotation
       at net.bytebuddy…AnnotationList$ForLoadedAnnotations.get(…)
```

So it is not a retransform defect at all — `CRATONVM_DBG_REDEFINE_DUMP`
captured nothing, because the retransform never started. ByteBuddy reads the
class's annotations first, and one of them is not an `Annotation`.

## Root cause

`QueryRequest` carries `@org.jboss.marshalling.Externalize`, and
**jboss-marshalling is not on the Spring Boot cache classpath.** HotSpot's
`sun.reflect.annotation.AnnotationParser` drops an annotation whose type will
not resolve; CratonVM surfaced it.

`resolvable_annotations` exists to do exactly what HotSpot does, and its doc
comment says so. It could not work: it resolves the type through
`resolve_annotation_type_class_id`, whose chain ends in `ctx.load_class`, and
`load_class` **fabricates a synthetic stand-in** for any name matching
`is_enterprise_stub_prefix` (`org/jboss/`, `org/infinispan/`, `io/quarkus/`, …)
that is on no classpath. The lookup therefore always succeeded, the filter could
essentially never say "unresolvable", and a `Proxy` was built over a stand-in
that is not even an interface — so the proxy does not implement
`java.lang.annotation.Annotation`.

Measured directly (`probes/AnnotationProxyIsAnnotationProbe.java`), the one bad
annotation against the eight good ones on the same class:

| | `isInterface` | `isAnnotation` | superinterfaces |
|---|---|---|---|
| `@org.jboss.marshalling.Externalize` (fabricated) | **false** | **false** | **[]** |
| the other 8 (`@ProtoTypeId`, `@ProtoField`, `@ProtoFactory`) | true | true | `[java.lang.annotation.Annotation]` |

`checked=9 bad=1` on CratonVM against `checked=8 bad=0` on HotSpot — the extra
one *is* the phantom.

## Why one class, and why it mattered anyway

`QueryRequest` is `public final`. For a final class the inline mock maker is the
only option, so its failure throws. For every **non-final** class Mockito
catches the same failure and silently falls back to the *subclass* mock maker,
which answers `mock()` happily — with a `Target$MockitoMock$…` proxy that cannot
intercept final methods or pre-existing instances. `QueryRequest` was not one
broken class; it was the only place a broken annotation read became visible.
`probes/MockMakerKindProbe.java` reports INLINE vs SUBCLASS per class so that
distinction is observable rather than inferred.

## Fix

`resolve_annotation_type_class_id` now requires the resolved class to *be* an
annotation type — `ACC_ANNOTATION`, or `java.lang.annotation.Annotation` among
its interfaces (JLS 9.6, both spellings of the same fact). Checking the resolved
class rather than "did this come from a stub?" also rejects a genuinely wrong
class of the same name, and states the invariant the proxy's consumers actually
depend on. Every caller goes through this one lookup, so the filter and the
builder cannot disagree.

## The fixture that proved nothing

The first regression fixture was a compile-only annotation in the **default
package** — and it passed on a pre-fix binary. `is_enterprise_stub_prefix` is
load-bearing: a missing default-package class is simply not found, so no
stand-in is fabricated and there is nothing to reproduce. The fixture only bites
with the annotation in `org/jboss/…`, which is why `AcpGoneSolo` now uses
`org.jboss.acpprobe.AcpGoneEnterprise`.

`AcpGoneProbe` also had two holes that would have hidden this:

* its `!(o instanceof Annotation)` branch **printed and moved on** without
  counting — the exact defect, treated as an observation;
* its `catch` around the first `getDeclaredAnnotations()` printed the summary
  and `return`ed from `main`, so on HotSpot (which throws there) every later
  case was skipped and the run still looked complete.

| arm | `AcpGoneSolo.getDeclaredAnnotations()` | summary |
|---|---|---|
| CratonVM, pre-fix | `count=1`, `non-annotation jdk.proxy1.$Proxy1` | `nonAnnotations=1` |
| **CratonVM, fixed** | `count=0` | `nonAnnotations=0` |
| HotSpot 25 | `count=0` | `nonAnnotations=0` |

## Verdict

| check | before | after |
|---|---|---|
| `mock(QueryRequest.class)` | MockitoException | **ok** |
| `AnnotationProxyIsAnnotationProbe` | `checked=9 bad=1` | **`checked=8 bad=0`** (identical to HotSpot) |
| `annotation_class_element_null` e2e | FAILED | **ok** |
| `cargo test -p cratonvm-native-builtins --lib` | — | 3292 passed, 0 failed |

The mock sweep that found it, same classpath, `MockManyProbe` over 1901 classes:

| arm | ok | fail | skip |
|---|---:|---:|---:|
| CratonVM, before the ThreadLocal fix | 636 | 92 | 1173 |
| CratonVM, after the ThreadLocal fix | 725 | 3 | 1173 |
| **CratonVM, after this fix** | **726** | **2** | 1173 |
| HotSpot 25 | 716 | 12 | 1173 |

The two remaining CratonVM failures are `ReflectionHintsExtensionsKt` and
`BeanDefinitionDsl` — Kotlin file-facade classes HotSpot refuses too.
