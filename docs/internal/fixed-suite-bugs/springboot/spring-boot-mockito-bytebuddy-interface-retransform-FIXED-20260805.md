# Mockito mock creation for multi-interface receivers — three null/cast sites, one funnel — RESOLVED

**Status: FIXED (2026-08-05).** Not a generic-signature/parameter-metadata gap
in CratonVM's reflection layer, and not receiver-shape-dependent. It is the
recycled-`JitInvokeInfo` dispatch aliasing fixed by `383e7f5cf`; see
`flywayautoconfigurationtests-timeout-jit-site-cache-aliasing-FIXED-20260805.md`
for the mechanism and
`mockito-bytebuddy-classfile-metadata-cluster-FIXED-20260805.md` for the
cluster-wide validation.

## Original symptom (as filed)

Three classes failed while Mockito's `InlineBytecodeGenerator` mocked an
interface or interface hierarchy, all funnelling through
`MockitoException: Could not modify all classes […]`, each with a different
underlying defect. HotSpot passed all three on the same fixture.

* `BatchJdbcAutoConfigurationTests` — `IllegalStateException:` (empty) →
  `ClassCastException: class java.lang.Integer cannot be cast to class java.lang.String`.
* `CassandraReactiveHealthContributorAutoConfigurationTests` — `IllegalStateException:` →
  `NPE: Cannot invoke "TypeDescription$Generic.accept(…)" because the return value of
  "TypeDescription$Generic$LazyProjection.resolve()" is null`.
* `XADataSourceAutoConfigurationTests` —
  `NPE: Cannot invoke "java.util.List.size()" because "this.parameterDescriptions" is null`.

## Root cause

`383e7f5cf`. Dispatch memos in `vm/src/jit/helpers.rs` are keyed on
`(vm_identity, JitInvokeInfo pointer)`; the boxes are freed with their
`CompiledMethod` and the address is re-issued to the next compile, so a compiled
site inherits the previous site's resolution.

The `BatchJdbcAutoConfigurationTests` face is the one that names the mechanism.
The trace bottoms out in

```
ClassCastException: class java.lang.Integer cannot be cast to class java.lang.String
  net.bytebuddy.matcher.ElementMatcher$Junction$ForNonNullValues.matches(ElementMatcher.java:252)
  net.bytebuddy.matcher.FilterableList$AbstractBase.filter(FilterableList.java:125)
  …ParameterAddingClassVisitor.visitMethod(InlineBytecodeGenerator.java:500)
```

`ElementMatchers.named(name).and(hasDescriptor(desc))` filtering
`typeDescription.getDeclaredMethods()`. The only checkcast that can raise that
message is `StringMatcher`'s `doMatch(Object)` bridge, so the argument to
`StringMatcher.matches` — i.e. the return of `MethodDescription.getActualName()`
or `getDescriptor()` — was a boxed `java.lang.Integer`. Both are `String`-typed
all the way down; no reflection gap in CratonVM can put an `Integer` there. An
aliased `NATIVE_SITE_CACHE` entry — the site calling the previous site's native
and returning its value — can, and does.

`LazyProjection.resolve()` returning `null` and `parameterDescriptions` being
`null` are the same memo returning a null instead of an object.

## Why the filed hypothesis was wrong, and why its diagnostic would not have found it

The page proposed that all three were ByteBuddy building a `TypeDescription`
from CratonVM's `Class.getGenericInterfaces()` /
`Method.getGenericParameterTypes()` / `Method.getParameters()`, by analogy with
two already-fixed generic-signature bugs, and prescribed:

1. grepping `native-builtins/src/lang_class.rs` and `lang_reflect_*.rs` for the
   Signature-attribute reifiers, and
2. a minimal non-Spring probe — `Mockito.mock(SomeMultiInterfaceType.class)` for
   each of the three interface shapes — to confirm the defect is
   **receiver-shape-dependent** (interface count / generic parameterization).

Both steps would have produced a confident wrong answer:

* The reifiers are correct. Reading them finds nothing, and "nothing found"
  after an analogy-driven search reads as "look harder", not "wrong hypothesis".
* **The minimal probe would have passed, and that pass would have been read as
  evidence.** The defect is not receiver-shape-dependent at all: it needs
  `CompiledMethod`s to be dropped and their info addresses re-issued, which a
  small standalone probe never does. `XADataSourceAutoConfigurationTests`
  itself passes 6/6 in isolation on the very binary this page was filed from —
  the class the page named as the cleanest `parameterDescriptions` witness is
  green when you run it alone.

The tell was the page's own sentence: *"a `ClassCastException`, not just a
null"*. A metadata gap yields absent or wrong-shaped data of the **right static
type**. A raw `Integer` in a `String` slot is a type-system violation, and only
dispatch can produce one.

## Validation

See `mockito-bytebuddy-classfile-metadata-cluster-FIXED-20260805.md` for the
cluster measurement. All three classes on this page are in that set.

| Class | Pre-fix `1078f6f05c`, isolation | Pre-fix, concurrent (6 rounds) | Arms with `383e7f5cf` |
|---|---|---|---|
| `BatchJdbcAutoConfigurationTests` | 34/34 clean | 0/6 bad | 34/34 every run |
| `CassandraReactiveHealthContributorAutoConfigurationTests` | 3/3 clean | **2/6 bad** — `MockitoException: Could not modify all classes [interface com.datastax.dse.driver.api.core.cql.reactive.ReactiveSession, …]` → `IllegalStateException` | 3/3 every run |
| `XADataSourceAutoConfigurationTests` | 6/6 clean | 0/6 bad | 6/6 every run |

Two of the three do not reproduce outside the full suite's compile churn even
under 13-way concurrency, which is the honest state of this page: their
retirement rests on the shared mechanism, on the cluster-level 16/78 → 0/78
before/after, and on the fact that the one face this page pinned down
(`Integer` in a `String` slot) is only producible by that mechanism.

## Affected classes

- `module/spring-boot-batch-jdbc` — `org.springframework.boot.batch.jdbc.autoconfigure.BatchJdbcAutoConfigurationTests`
- `module/spring-boot-cassandra` — `org.springframework.boot.cassandra.autoconfigure.health.CassandraReactiveHealthContributorAutoConfigurationTests`
- `module/spring-boot-jdbc` — `org.springframework.boot.jdbc.autoconfigure.XADataSourceAutoConfigurationTests`
