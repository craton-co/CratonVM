# Elasticsearch Byte Buddy annotated-owner reflection mismatch

Status: ✅ FIXED (branch `fix/es-bytebuddy-annotatedtype`)

Date observed: 2026-07-02
Date fixed: 2026-07-02

## Summary

Elasticsearch server tests failed under CratonVM when Mockito/Byte Buddy created a
mock and copied generic type-annotation metadata. HotSpot passed the same
representative classes with the same classpath.

Failure signature:

```text
Mockito cannot mock this class: class org.elasticsearch.index.query.SearchExecutionContext.
Underlying exception : java.lang.IllegalArgumentException:
object of type net.bytebuddy.description.type.TypeDescription$Generic$AnnotationReader$NoOp
is not an instance of java.lang.reflect.AnnotatedType
```

## Root cause

CratonVM's `Method.getAnnotatedReturnType()` / `Parameter.getAnnotatedType()` /
`Executable.getAnnotatedParameterTypes()` / `Field.getAnnotatedType()`
(`native-builtins/src/lang_class.rs`) had two compounding bugs:

1. **They wrapped the *erased* type, not the *generic* one.** They built the
   backing `Type` from `getReturnType()`/`getType()` (a plain `Class`) instead
   of `getGenericReturnType()`/`getGenericParameterTypes()`/`getGenericType()`
   (a real `ParameterizedType`/`GenericArrayType`/`TypeVariable`/`WildcardType`
   when the member is generic, built from the parsed Signature attribute).
   So a method returning e.g. `Map<String, MappedFieldType>` produced an
   `AnnotatedType` wrapping plain `Map.class`, discarding all type-argument /
   owner-type information HotSpot's real object carries.

2. **`make_annotated_type`/`make_annotated_type_with_anns` always constructed
   `sun.reflect.annotation.AnnotatedTypeFactory$AnnotatedTypeBaseImpl`**,
   regardless of the backing `Type`'s actual kind. Real JDK's
   `AnnotatedTypeFactory.buildAnnotatedType()` instead dispatches to one of
   four subclasses — `AnnotatedArrayTypeImpl` / `AnnotatedTypeVariableImpl` /
   `AnnotatedParameterizedTypeImpl` / `AnnotatedWildcardTypeImpl` — each
   implementing the matching `AnnotatedXxxType` sub-interface
   (`AnnotatedParameterizedType`, etc.). CratonVM's objects only ever
   satisfied plain `AnnotatedType`.

Byte Buddy's `JavaDispatcher` mirrors JDK reflection: `AnnotationReader
.ForOwnerType`'s static `ANNOTATED_TYPE` proxy field reflectively calls
`AnnotatedType.getAnnotatedOwnerType()` on whatever object the (Byte-Buddy-
internal) type-argument/owner reader chain resolved to. When an *earlier*
step in that chain needed the object to behave as `AnnotatedParameterizedType`
(e.g. to read `getAnnotatedActualTypeArguments()`, only declared on that
sub-interface) and it didn't — because of bug #2, compounded by bug #1
producing the wrong kind of `Type` to begin with — Byte Buddy's own
`ClassCastException`-catching fallback substituted its `NoOp` "no annotation
reader" sentinel. That `NoOp` sentinel was then fed into the *next* level's
`getAnnotatedOwnerType()` call, and — because `NoOp` doesn't implement
`AnnotatedType` at all — CratonVM's `Method.invoke()` receiver-type check
(itself a real, correct HotSpot-matching check) legitimately raised
`IllegalArgumentException`, which Byte Buddy's owner-type reader does *not*
catch (only `ClassCastException`), so it escaped as a raw `IllegalArgumentException`
into Mockito's mock-creation path.

(An initial hypothesis blamed HotSpot's reflection "MethodAccessor inflation"
— the switch from `NativeMethodAccessorImpl` to a bytecode-generated accessor
after ~15 calls through the same `Method`, which does change some JDK
receiver-mismatch behavior. Empirical testing against a real JDK 25 disproved
this for `Method.invoke()`: it always raises `IllegalArgumentException` for a
type-mismatched receiver, regardless of prior call count. The real bug was
the impl-class/erasure gap above.)

## Fix

`native-builtins/src/lang_class.rs`:
- Added `annotated_type_impl_class_name()`, selecting the correct
  `AnnotatedTypeFactory` impl class for a backing `Type` (array → `Array`,
  `TypeVariable` → `TypeVariable`, `ParameterizedType` → `Parameterized`,
  `WildcardType` → `Wildcard`, else the base impl) — mirroring
  `AnnotatedTypeFactory.buildAnnotatedType()`'s own dispatch. All four
  subclasses extend the base impl and declare no extra fields, so the
  existing 4-field layout (`type`, `location`, `allOnSameTargetTypeAnnotations`,
  `annotations`) is reused unchanged.
- `make_annotated_type` / `make_annotated_type_with_anns` now call it instead
  of hardcoding the base impl class.
- `native_method_get_annotated_return_type`, `native_parameter_get_annotated_type`,
  `native_executable_get_annotated_parameter_types`, and
  `native_field_get_annotated_type` now resolve the GENERIC type
  (`getGenericReturnType()` / `getGenericParameterTypes()` / `getGenericType()`)
  as the backing `Type`, falling back to the erased type only when the
  generic call is unavailable (non-generic member) or the parameter index is
  out of range.

## Verification

- `cargo test -p cratonvm-native-builtins` (`lang_class::` module, 140 tests;
  full crate suite) — all pass, no regressions.
- `org.elasticsearch.action.bulk.ShardBatchMapperResolveTests` (the doc's
  original repro, `all` category, data row 434 — the list shifted by 5 rows
  since the bug was filed): the `IllegalArgumentException`/`NoOp`/`AnnotatedType`
  signature no longer appears anywhere in the log; the test now runs the
  actual `RandomizedRunner` suite machinery (44s vs. an 18s immediate mock
  failure before the fix). It still exits non-zero, but only because of an
  unrelated, pre-existing gap: `Module.getDescriptor()` returns null in
  `XContentProvider$Holder.<clinit>` (`NullPointerException: Cannot invoke
  "java.lang.module.ModuleDescriptor.uses()" ...`) — confirmed independently
  affecting `org.elasticsearch.action.bulk.Retry2Tests` too, i.e. a
  suite-bootstrap-wide JPMS gap unrelated to Mockito/Byte Buddy. Not in scope
  for this fix; worth its own follow-up.
- Spot-checked 4 more of the originally-reported classes (`FieldCapabilitiesFilterTests`,
  `MetadataDataStreamsServiceTests`, `BatchDocumentParserContextTests`,
  `BooleanFieldBlockLoaderTests`): 0/4 show the old signature;
  `BooleanFieldBlockLoaderTests` now fully **PASSes**; the other 3 hit the
  same unrelated `Module.getDescriptor()` gap above.
- Also verified `MetadataMigrateToDataStreamServiceTests` (the 4th no-JIT-run
  class below): 0 matches of the old signature; hits the same unrelated
  `Module.getDescriptor()` gap, confirming the fix covers all 6 classes
  checked across both the JIT and no-JIT partial-evidence runs.

## Repro (pre-fix; kept for reference)

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 429 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-bytebuddy-annotatedtype-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

(Note: by the time of the fix, `ShardBatchMapperResolveTests` had shifted to
data row 434 in a freshly regenerated class list — a few classes were
added/reordered upstream between the two sessions. Re-derive the row via
`grep -n '<ClassName>$' apps\elasticsearch-suite-runner\.suite\all-tests.tsv`
rather than trusting a hardcoded `-Start` value.)

## No-JIT partial evidence (pre-fix)

Run `es-nojit-full-20260702` was stopped by request after 1366 recorded
classes. The partial no-JIT run had already hit 4 CratonVM-only failures with
this same Byte Buddy signature — confirming the bug was JIT-independent (as
expected, since the root cause was in native reflection object construction,
not JIT codegen):

```text
index=426 org.elasticsearch.action.bulk.ShardBatchMapperResolveTests
index=456 org.elasticsearch.action.fieldcaps.FieldCapabilitiesFilterTests
index=753 org.elasticsearch.cluster.metadata.MetadataDataStreamsServiceTests
index=766 org.elasticsearch.cluster.metadata.MetadataMigrateToDataStreamServiceTests
```

Three of these four overlap with classes already spot-checked post-fix above
(`ShardBatchMapperResolveTests`, `FieldCapabilitiesFilterTests`,
`MetadataDataStreamsServiceTests`); `MetadataMigrateToDataStreamServiceTests`
was separately re-verified too (see Verification above) — 0 matches of the
old signature post-fix.

Evidence:

```text
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-nojit-full-20260702\all-nojit\results.tsv
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.action.bulk.ShardBatchMapperResolveTests.err.log
C:\craton\CratonVM-es-bytebuddy-annotatedtype\apps\elasticsearch-suite-runner\.suite\results\es-bytebuddy-annotatedtype-realfix\all-jit\logs\server.org.elasticsearch.action.bulk.ShardBatchMapperResolveTests.err.log  (post-fix)
```
