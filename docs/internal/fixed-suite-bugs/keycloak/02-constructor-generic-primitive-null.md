# 02 — `Constructor.getGenericParameterTypes()` returns null for a primitive parameter

**Status:** FIXED (worktree `CratonVM-kcsuite`)
**Affected keycloak classes (2):** CredentialModelTest (`canDeserializeMinimalJson`),
CredentialModelBackwardsCompatibilityTest (`testCredentialModelPassword`)
**Surface symptom:** `RuntimeException: com.fasterxml.jackson.databind.exc.InvalidDefinitionException: Unrecognized Type: [null]`

## Repro (minimal, no keycloak)
```java
class Foo { Foo(int a, String b, Map<String, List<String>> c) {} }
Constructor<?> c = Foo.class.getDeclaredConstructors()[...]; // the 3-arg one
Type[] gpt = c.getGenericParameterTypes();
// HotSpot : [int, class java.lang.String, Map<String,List<String>>]
// CratonVM: [null, class java.lang.String, ParameterizedType@..]   <-- index 0 is null
```

## Root cause
The constructor carries a generic `Signature` attribute (because of the `Map<…>` param),
so CratonVM resolves parameter types from the parsed signature via
`generics.rs::type_sig_to_java`. For a primitive base type (`TypeSig::Base('I')`) it did:
```rust
if let Some(cid) = ctx.class_id_by_name("int") { ... } else { Value::Object(None) }
```
`class_id_by_name` returns `None` for primitives (they have no regular `ClassId`), so the
`int` parameter collapsed to **null**. Jackson's `TypeFactory` then hit a null `Type`
while resolving the `@JsonCreator` constructor of `PasswordCredentialData(int hashIterations,
String algorithm, Map<String,List<String>> additionalParameters)` and threw
`InvalidDefinitionException: Unrecognized Type: [null]`.

## Fix
Use the dedicated primitive-mirror accessor (as `jmx_openmbean.rs` already does):
```rust
TypeSig::Base(ch) => {
    let prim_name = match ch { 'I' => "int", 'J' => "long", /* … */ _ => return Value::Object(None) };
    Value::Object(Some(ctx.primitive_class_mirror(prim_name)))
}
```
File: `native-builtins/src/generics.rs::type_sig_to_java`. General win: any generic
method/ctor/field signature that mixes a primitive with a parameterized type now reifies
the primitive correctly.
