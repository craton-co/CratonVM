# HIB-CV-08 — `Constructor.newInstance()` on a no-arg ctor adopts a *different* constructor's parameter list → entity instantiation fails

**Severity:** High — fails every Hibernate entity whose class declares a real no-arg constructor **and** a unique public constructor with parameters (composite-id key classes, `@IdClass`, enum-mapped, key-many-to-one). ≥7 classes in the first 215 census classes.
**Status:** ✅ FIXED (worktree `fix/hibernate-full-suite`, `native-builtins/src/lang_class.rs`)
**Mode:** Interpreter (JIT-off census) — not a JIT bug.
**HotSpot:** not affected.

## Symptom

```
org.hibernate.InstantiationException: Could not instantiate entity
  'org.hibernate.orm.test.annotations.cid.keymanytoone.Card'
Caused by: java.lang.IllegalArgumentException: Constructor.newInstance: wrong number of
  arguments for org/.../keymanytoone/Card: expected 1, got 0
   at org.hibernate.metamodel.internal.EntityInstantiatorPojoStandard.instantiate(...:98)
```

Affected classes (census, growing): `EagerKeyManyToOneTest` (×2), `CompositeIdFkGeneratedValueTest`,
`CompositeIdDerivedIdWithIdClassTest`, `EnumeratedAndConvertorTest`, `MapKeyEnumeratedTest`,
`IdMapManyToOneSpecjTest`, `IdClassGeneratedValueManyToOneTest`, …

## Root cause

`Card` declares two constructors: a package-private **no-arg** (`mods=0`) and a public **`Card(String)`**
(`mods=1`). Hibernate's `EntityInstantiatorPojoStandard` instantiates via the no-arg constructor:
`constructor.newInstance()` with **0 args**.

Reflection metadata is correct — `getDeclaredConstructor().getParameterCount()` returns **0** on
CratonVM. But `Constructor.newInstance` re-derives the parameter descriptor in
`constructor_descriptor_for_new_instance` (`lang_class.rs`), and that path had a bug:

* `compose_init_descriptor_from_parameter_types` returns `"()V"` for **two** different situations —
  a *genuine* no-arg constructor (present-but-empty `parameterTypes` array) **and** a read-failure
  (the `parameterTypes` field couldn't be read at all).
* A fallback then says: "if `composed == "()V"`, recover the descriptor from the class's **unique
  public `<init>`**." That fallback was intended only for the read-failure case (e.g. Surefire's
  `JUnitPlatformProvider(ProviderParameters)`).
* For `Card`, the no-arg ctor *legitimately* composes `"()V"`, but the class's **only public** ctor
  is `Card(String)`, so the fallback substituted `"(Ljava/lang/String;)V"` → expected 1 param →
  `newInstance()` with 0 args throws "expected 1, got 0".

So a real no-arg constructor inherited the parameter list of the class's other (public) constructor.

## Fix

Only apply the unique-public-`<init>` fallback when `parameterTypes` was **genuinely unreadable**,
i.e. the field is not a present array. A present-but-empty `parameterTypes` array means a real
no-arg constructor and must keep `"()V"`:

```rust
if composed == "()V" {
    let param_types_readable = matches!(
        ctx.get_field_by_name(ctor_obj, "parameterTypes"),
        Value::Object(Some(_)));
    if !param_types_readable { /* …unique_public_init_descriptor fallback… */ }
}
```

The Surefire read-failure case (`parameterTypes` absent) still recovers via the fallback.

## Repro

`ConProbe` (in `.cratonvm-suite/`): loads `...keymanytoone.Card`, lists ctors (`0`-arg + `String`),
then `getDeclaredConstructor().newInstance()`. Before: `IllegalArgumentException expected 1, got 0`.
After the fix: constructs `Card` successfully (matches HotSpot).
