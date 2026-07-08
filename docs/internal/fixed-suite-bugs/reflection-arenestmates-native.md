# Reflection.areNestMates native registration

Status: FIXED 2026-07-08 on `dev`.

## Symptom

Spring `DefaultListableBeanFactoryTests.beanProviderSerialization()` failed in
real-JDK mode with JIT enabled when the JDK serialization path called:

```text
java.lang.UnsatisfiedLinkError: jdk/internal/reflect/Reflection.areNestMates(Ljava/lang/Class;Ljava/lang/Class;)Z
```

## Cause

`jdk/internal/reflect/Reflection.areNestMates(Class, Class)` was not registered
in CratonVM's real-JDK essential native set, so the library access-check path
failed before it could compare nest hosts.

## Fix

`native-builtins/src/lib.rs` registers
`Reflection.areNestMates(Class, Class)` next to the other
`jdk.internal.reflect.Reflection` natives. The implementation delegates host
resolution to the same `Class.getNestHost0` native helper used by
`Class.getNestHost()`, so lambda proxy and class metadata behavior stays
consistent across the two reflective entry points.

`vm/tests/wp2_1_reflect.rs` now pins the registration in
`register_essential_natives()` so future registration-order or merge churn
cannot drop this native silently.

## Verification

- `cargo test -p cratonvm-vm --test wp2_1_reflect jdk_internal_reflection_are_nest_mates_registered`
- Spring suite rerun of `DefaultListableBeanFactoryTests` with a fresh unique Azure binary confirmed the `areNestMates` `UnsatisfiedLinkError` is absent.
