# FIXED: Atomic intrinsics missed call sites that name a subclass

**Status: FIXED 2026-09-12.** This closes the residual of the 2026-09-12 JIT
review finding #80, "Intrinsics resolve String layout twice per compile, match
only the exact CP class, and read an env var per call". The String layout and
env var parts were already fixed.

## The defect

```java
class Counter extends AtomicInteger {}
...
counter.incrementAndGet();   // CP class: Counter, not AtomicInteger
```

The constant-pool method ref names `Counter`. The ATOMIC_INT / ATOMIC_LONG
registrations compared that name against
`java/util/concurrent/atomic/AtomicInteger`. The names differ, so the call paid
full dispatch instead of the inline `LOCK XADD` / CAS sequence.

This happened at both doors that register the family: `try_compile_inner` in
`jit/src/lib.rs`, and the OSR door `compile_osr_artifact` in
`vm/src/runtime/interpreter/jit_bridge.rs`.

The subclass matcher (`try_resolve_atomic_*_intrinsic_for_site` with a
`resolved_declaring_class`) existed and was tested. Nothing supplied its input.

## The fix

### A declaring-class resolver on the compile request

`try_compile_with_invokespecial_resolver` and `try_compile_inner` take a new
trailing parameter:

```rust
cp_invoke_declaring_class_resolver: Option<&dyn Fn(u16) -> Option<String>>
```

It maps an invoke CP index to the name of the class that DECLARES the method
the site resolves to. `try_compile` passes `None`, because it has no VM. It is
a different question from `cp_invokespecial_owner_resolver`, which answers
only for statically bound sites and returns `None` exactly when a native
screen refuses. That makes it useless for the Atomic family.

### The VM supplies it

The VM side is `jit_bridge.rs::cp_method_ref_declaring_class_name(shared,
holder, cp_idx)`. It reads the `Methodref`, resolves the CP class in `holder`'s
loader with `find_class_by_name_for_class`, and walks up with
`classloading::find_method_recursive`, the same walk
`try_jit_compile_callee_slow` uses. It returns the declaring class's name.

All three `try_compile_with_invokespecial_resolver` doors pass a closure over
it:

- `compile_optimizing_artifact`
- its eager callee compile
- `try_jit_compile_callee_slow`

### The registration

Both regions in `try_compile_inner` call `atomic_intrinsic_for_invoke_site`,
which works in four steps:

1. `atomic_intrinsic_site_may_match` is a cheap pre-filter. It admits the JDK
   class itself, or any class calling a method `final` in the JDK class. Other
   sites never reach the class-id or declaring-class resolvers, so a `get()I`
   on an unrelated class pays no lookup.
2. `guard_class_id` comes from `cp_invoke_class_id_resolver`, which is the
   site's own class (`Counter`).
3. The declaring-class resolver is asked only for a non-JDK CP class.
4. `try_resolve_atomic_*_intrinsic_for_site` admits the site only when the
   declaring class IS the JDK class and the method is `final` there. That
   excludes `AtomicLong.longValue()` and any intermediate override.

The OSR door does the same through the class-manager guard its invoke loop
already holds (`cm_lock`), via `site_class_and_declaring_class_name`. It never
takes a second `.read()`; see
`vm-cli/tests/jit_compile_gate_doors.rs::the_osr_door_takes_no_recursive_class_manager_lock`.
Exact-class sites keep the bootstrap `AtomicInteger` / `AtomicLong` id as
before.

## Why the inline sequence is sound on a subclass instance

- **The field slot is preserved.** `compute_field_layout`
  (`classloading/src/class_manager.rs`) gives superclass instance fields the
  slots `0..N` and starts a class's own fields at `N`. `AtomicInteger.value` /
  `AtomicLong.value` is slot 0 in every subclass, which is what
  `AtomicIntFieldLayout::new(0, ..)` / `AtomicLongFieldLayout::new(0, ..)`
  address.
- **The layout is the subclass's own.** Both layouts are built from
  `guard_class_id`, the SUBCLASS's id, in the matcher and again in the x64
  walker. The compact offset therefore comes from the subclass's own registered
  compact layout. A storage width other than 4 or 8 bytes refuses the site.
- **The guard is the receiver's expected class.** The emitted guard compares
  the receiver header's class id exactly against the subclass. A receiver of
  any other class, including another subclass, takes the existing mismatch
  path.
- **The body cannot be replaced.** The method is `final` in the JDK, so no
  subclass can override it.

## Regression coverage

`jit/src/lib.rs`, module `atomic_accessor_intrinsic_tests`:

- `a_subclass_site_registers_through_the_declaring_class_resolver` exercises
  the resolver-driven registration:
  - a subclass site registers for both families, guarded on the site's own id;
  - it does not register with no resolver, with a resolver naming an
    intermediate class, for the overridable `longValue()`, or with no class id;
  - the resolver is not consulted for an exact JDK site or for a non-final
    method.
- Existing: `subclass_site_of_a_final_jdk_method_matches_through_the_declaring_class`,
  `subclass_site_of_an_overridable_jdk_method_is_not_intrinsified`.

`jit/tests/ir_vs_singlepass.rs` passes `None` for the new parameter.

## Not covered by this fix

Both gaps this section used to list are closed too.

- **The final-devirt rewrite now yields to the Atomic ladders.**
  `try_compile_inner` rewrites an `invokevirtual` of a `final` method to a
  static bind (`invoke_kind = 1`), which skips the intrinsic gate. Before, only
  the native screen in `invokevirtual_site_final_owner` kept Atomic sites at
  `invoke_kind == 0`, because the RMW methods have registered natives. A method
  of the family with no native, or `CRATONVM_JIT_FINAL_DEVIRT_NATIVE_SCREEN=0`,
  would have been pinned to a static bind. The yield now also asks
  `site_yields_to_atomic_intrinsic`, which calls
  `atomic_intrinsic_for_invoke_site` exactly as the registration does, for both
  families, exact and subclass sites alike.
- **The IR tier matches subclass sites.** `ir_unbox_match_class` hands
  `ir::try_ir_unbox_intrinsic` the JDK class name for a subclass site whose
  method the declaring-class resolver places in the JDK class, `final` there.
  The site's own class id stays the layout source and the guard. The lowered
  `Op::Unbox` compares the receiver's class id exactly, so any other receiver
  deopts.

Tests: `the_final_devirt_rewrite_yields_to_an_atomic_subclass_site` and
`ir_unbox_sites_of_an_atomic_subclass_match_as_the_jdk_class` (`jit/src/lib.rs`).
No VM-level probe runs a `Counter extends AtomicInteger` loop; the registration,
yield and IR match are each covered at the unit level.
