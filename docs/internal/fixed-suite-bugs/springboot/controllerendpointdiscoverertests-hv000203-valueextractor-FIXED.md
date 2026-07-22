# Hibernate Validator rejects `ArgumentValueValueExtractor` (`HV000203`) — `ControllerEndpointDiscovererTests`

**Status: FIXED 2026-07-18** (branch `fix/hv000203-valueextractor-20260717`)

## Symptom

```
JUnit Jupiter:ControllerEndpointDiscovererTests:<2 test methods>
  => java.lang.IllegalStateException: Unstarted application context ...[startupFailure=BeanCreationException] failed to start
   Caused by: org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'defaultValidator' defined in ...$ProxyBeanConfiguration: HV000203: Value extractor type org.springframework.graphql.data.method.annotation.support.ArgumentValueValueExtractor fails to declare the extracted type parameter using @ExtractedValue.
   Caused by: jakarta.validation.valueextraction.ValueExtractorDefinitionException: HV000203: Value extractor type org.springframework.graphql.data.method.annotation.support.ArgumentValueValueExtractor fails to declare the extracted type parameter using @ExtractedValue.
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-actuator.org.springframework.boot.actuate.endpoint.web.annotation.Controlle-23f2a1805bec.out.log`
(both failing test methods hit the identical exception during
`defaultValidator` bean creation.)

## What the class under scrutiny looks like (confirmed via `javap` on the real jar)

`org.springframework.graphql.data.method.annotation.support.ArgumentValueValueExtractor`
(`spring-graphql-2.0.4-SNAPSHOT.jar`):

```java
public final class ArgumentValueValueExtractor
    implements jakarta.validation.valueextraction.ValueExtractor<org.springframework.graphql.data.ArgumentValue<?>>
```

`javap -v` on the real class file shows the actual annotation site precisely:

```
RuntimeVisibleTypeAnnotations:
  0: #52(): CLASS_EXTENDS, type_index=0, location=[TYPE_ARGUMENT(0), TYPE_ARGUMENT(0)]
    jakarta.validation.valueextraction.ExtractedValue
```

i.e. `implements ValueExtractor<ArgumentValue<@ExtractedValue ?>>` — the
annotation targets the wildcard nested **two levels deep** inside the
implemented interface's type argument.

## Root cause (confirmed)

Two independent gaps in CratonVM's reflection machinery, both required to
reproduce HV000203 — Hibernate Validator's `ValueExtractorResolver` walks
`Class.getAnnotatedInterfaces()` then chains
`AnnotatedParameterizedType.getAnnotatedActualTypeArguments()` twice to reach
the wildcard and check for `@ExtractedValue`:

1. **The class-level `RuntimeVisibleTypeAnnotations` attribute (JVMS 4.7.20
   `CLASS_EXTENDS` target, `0x10`) was discarded during class loading.**
   `classloading::class_manager`'s per-class-attribute fold (the loop that
   populates `signature`/`nest_host`/`record_components`/etc. on the `Class`
   struct) never recognized this attribute, so by the time
   `Class.getAnnotatedInterfaces()` / `getAnnotatedSuperclass()` ran, the data
   was already gone — there was nowhere to read it from.
2. **Even where TYPE_USE annotation plumbing existed (method return types,
   parameters, fields), it only modeled a single `TYPE_ARGUMENT` nesting
   level.** `getAnnotatedInterfaces()` itself always attached an
   unconditionally *empty* `Annotation[]` (`make_annotated_type`, no
   annotation lookup at all), and the one existing extraction path
   (`extract_return/parameter/field_type_argument_annotations`) explicitly
   matched only a `type_path` of length exactly 1 — a nested case like
   `ArgumentValue<@ExtractedValue ?>` (`type_path` length 2) had no
   representation anywhere.

Fix (`native-api/src/registry.rs`, `vm/src/vm/vm_exec.rs`,
`native-builtins/src/lang_class.rs`):

- Added `TypeArgAnnotations`, a tree type (`anns` + per-argument `children`)
  that models TYPE_USE annotations at **arbitrary nesting depth**, replacing
  the old flat `Vec<Vec<AnnotationData>>` shapes for the method-return/
  parameter/field type-argument extraction paths.
- Added `NativeContext::class_extends_type_annotations(class_id,
  supertype_index)`, which recovers the otherwise-discarded class-level
  attribute by re-parsing the class's cached original bytes
  (`class_bytes_cache`, already retained for JVMTI retransformation) on the
  rare occasions a framework actually calls `getAnnotatedInterfaces()` /
  `getAnnotatedSuperclass()` — avoiding a new field threaded through every
  `Class` construction site in the codebase.
- Wired this into `Class.getAnnotatedInterfaces()` / `getAnnotatedSuperclass()`
  (annotations directly on the supertype) and made
  `AnnotatedParameterizedType.getAnnotatedActualTypeArguments()`
  self-sustaining: each call re-stashes its own argument's remaining
  `children` subtree, so calling it again on the nested result (exactly what
  Hibernate Validator does here) descends one more level instead of
  dead-ending after one.

## Verification

- Standalone probe (`Hv203Probe.java`, both a depth-1 `Comparable<@Marker
  String>` case and a depth-2 `Extractor<Wrapper<@Marker ?>>` case mirroring
  `ValueExtractor<ArgumentValue<@ExtractedValue ?>>` exactly): matches real
  HotSpot output (`PROBE_PASS`) under the fixed CratonVM build.
- `ControllerEndpointDiscovererTests` under CratonVM: 8/8 tests pass (was
  failing at context startup with HV000203).
- `cargo test --release -p cratonvm-native-builtins`: 3001 passed, 0 failed
  (no regressions from the `TypeArgAnnotations` refactor).

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-actuator` | `org.springframework.boot.actuate.endpoint.web.annotation.ControllerEndpointDiscovererTests` (both failing test methods) — now 8/8 PASS |

See also [`graphql-hibernate-validator-valueextractor-annotatedtype-gap-FIXED.md`](graphql-hibernate-validator-valueextractor-annotatedtype-gap-FIXED.md) — the same root cause, fixed in the same change.
