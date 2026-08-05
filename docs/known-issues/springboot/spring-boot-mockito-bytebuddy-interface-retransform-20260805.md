# Mockito mock creation for multi-interface receivers fails inside ByteBuddy retransform — 3 distinct null/cast sites, same "Could not modify all classes" funnel

**Status: OPEN — found 2026-08-05**

## Symptom

All three failures happen while Mockito's `InlineBytecodeGenerator` mocks an
interface (or interface hierarchy) via ByteBuddy's `TypeCache.findOrInsert` →
`retransformClasses`, and all three surface as
`org.mockito.exceptions.base.MockitoException: Could not modify all classes
[...]`, but each has a *different* underlying defect. HotSpot passes all
three classes cleanly on the same fixture. Given Mockito inline-mock
retransformation was previously a known trouble spot on CratonVM (see
`spring-boot-groovy-indy-mockito-mock-dispatch.md`, layers h1/h2, both
FIXED), these look like further, previously-unseen gaps in the same general
area rather than a regression of that doc (different exceptions entirely —
no `addReads0` `UnsatisfiedLinkError`, no empty `ConcurrentHashMap`).

### `BatchJdbcAutoConfigurationTests`
```
MockitoException: Could not modify all classes [interface org.springframework.batch.core.launch.JobLauncher,
  interface org.springframework.batch.core.launch.JobOperator]
Caused by: java.lang.IllegalStateException:
Caused by: java.lang.ClassCastException: class java.lang.Integer cannot be cast to class java.lang.String
```

### `CassandraReactiveHealthContributorAutoConfigurationTests`
```
MockitoException: Could not modify all classes [... 11 CqlSession-family interfaces ...]
Caused by: java.lang.IllegalStateException:
Caused by: java.lang.NullPointerException: Cannot invoke
  "net.bytebuddy.description.type.TypeDescription$Generic.accept(net.bytebuddy.description.type.TypeDescription$Generic$Visitor)"
  because the return value of "net.bytebuddy.description.type.TypeDescription$Generic$LazyProjection.resolve()" is null
```

### `XADataSourceAutoConfigurationTests`
```
MockitoException:
Underlying exception : java.lang.NullPointerException: Cannot invoke "java.util.List.size()"
  because "this.parameterDescriptions" is null
```

## Root cause

Not identified at the source level. All three failures are ByteBuddy
building a `TypeDescription` for a mocked-interface's method/type-parameter
metadata (generic signatures, parameter descriptions) while walking a
multi-interface receiver, and getting a null or wrong-typed value from
CratonVM's reflection layer partway through. This is consistent with the
same class of bug documented for `getGenericInterfaces`/generic-signature
resolution elsewhere in this codebase
(`instanttodateconverter-generic-interface-resolution-cluster-FIXED-20260729.md`,
`lambda-getgenericinterfaces-fabricates-parameterizedtype-FIXED.md`) but not
confirmed to share an exact code-level cause — the three null/cast sites
here are different enough (a `ClassCastException`, not just a null; a
`TypeDescription$Generic$LazyProjection` resolving to null; a `Method`'s
parameter-descriptor list being null) that this may be 2-3 separate defects
funneling through the same Mockito retransform call site rather than one.

## Where to look next

`net.bytebuddy.description.type.TypeDescription$Generic$LazyProjection` and
`TypeDescription.Generic.AnnotationReader` construction depend on
`Class.getGenericInterfaces()`/`Method.getGenericParameterTypes()`/
`Method.getParameters()` reflection — grep
`native-builtins/src/lang_class.rs` and `native-builtins/src/lang_reflect_*.rs`
for the Signature-attribute reifiers backing those. Isolate with a minimal
non-Spring probe: `Mockito.mock(SomeMultiInterfaceType.class)` for each of
the three interface shapes above, outside Spring Boot, to confirm the defect
is receiver-shape-dependent (interface count / generic-parameterization)
rather than Spring-specific.

## Affected classes

- `module/spring-boot-batch-jdbc` — `org.springframework.boot.batch.jdbc.autoconfigure.BatchJdbcAutoConfigurationTests`
- `module/spring-boot-cassandra` — `org.springframework.boot.cassandra.autoconfigure.health.CassandraReactiveHealthContributorAutoConfigurationTests`
- `module/spring-boot-jdbc` — `org.springframework.boot.jdbc.autoconfigure.XADataSourceAutoConfigurationTests`
