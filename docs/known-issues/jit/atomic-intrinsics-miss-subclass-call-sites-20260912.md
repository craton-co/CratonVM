# Atomic intrinsics miss call sites that name a subclass

**Status:** OPEN (missed optimization). This is the residual of the 2026-09-12
JIT review finding "Intrinsics resolve String layout twice per compile, match
only the exact CP class, and read an env var per call". The other two parts of
that finding are fixed: the String layout is resolved once per compile, and
the env var is read once.

## Shape

```java
class Counter extends AtomicInteger {}
...
counter.incrementAndGet();   // CP class: Counter, not AtomicInteger
```

The constant-pool method ref names `Counter`. The ATOMIC_INT / ATOMIC_LONG
matchers in `jit/src/lib.rs` compare that name against
`java/util/concurrent/atomic/AtomicInteger`. The names differ, so the call
never becomes the inline XADD/CAS sequence and pays full dispatch instead.

## What exists

The matching logic is written and unit-tested, but nothing calls it in
production:

- `try_resolve_atomic_intrinsic_for_site` and
  `try_resolve_atomic_long_intrinsic_for_site` take a
  `resolved_declaring_class`.
- The old four-argument matchers delegate to them with `None`, so today's
  behavior is exact-class only.
- `atomic_intrinsic_site_class_matches` admits a subclass site only when both
  of these hold:
  - the declaring class is the JDK class;
  - the method is `final` in the JDK. The read-modify-write family qualifies;
    `AtomicLong.longValue()` is excluded because it is overridable.
- Tests in `atomic_accessor_intrinsic_tests`:
  - `subclass_site_of_a_final_jdk_method_matches_through_the_declaring_class`;
  - `subclass_site_of_an_overridable_jdk_method_is_not_intrinsified`.

## What is missing

`try_compile` has no resolver that answers "which class declares the method
this constant-pool entry resolves to". The fix:

1. Add a `Fn(u16) -> Option<String>` resolver next to
   `cp_invokespecial_owner_resolver`. It maps a CP method-ref index to the
   declaring class of the resolved method, and the VM side fills it from its
   method resolution.
2. Pass the answer into the ATOMIC_INT / ATOMIC_LONG registration sites. A
   comment marks the spot.
3. Before enabling, confirm that the inherited `value` field sits at the same
   slot in a subclass instance. The inline sequence addresses slot 0 through
   `AtomicIntFieldLayout` / `AtomicLongFieldLayout`, and a subclass that adds
   fields must not move it.
