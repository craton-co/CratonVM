# HIB-CV-07 — Synthetic `java.lang.reflect.TypeVariable` violates `equals`/`hashCode` contract → Bean Validation "No validator could be found"

**Severity:** High — fails every Hibernate test that uses Jakarta Bean Validation constraints (`@Max`/`@Min`/… on `BigDecimal`/`BigInteger`/…). ≥5 classes in the first 215 census classes.
**Status:** ✅ FIXED (worktree `fix/hibernate-full-suite`, `native-builtins/src/lang_reflect.rs`)
**Mode:** Interpreter (JIT-off census) — not a JIT bug.
**HotSpot:** not affected.

## Symptom

```
jakarta.validation.UnexpectedTypeException: HV000030: No validator could be found
  for constraint 'jakarta.validation.constraints.Max' validating type 'java.math.BigDecimal'.
  Check configuration for 'radius'
```

Affected classes (census, growing): `BeanValidationAutoTest`, `BeanValidationGroupsTest`,
`BeanValidationProvidedFactoryTest`, `CollectionActionsValidationTest`,
`HibernateTraversableResolverTest`, …

DDL generation already emits the `@Max` check constraint (`check ((radius<=10))`), so the
constraint is parsed; the failure is at **runtime validation**, when Hibernate Validator resolves
*which* `ConstraintValidator` implementation handles `@Max` for a `BigDecimal` field.

## Root cause

Hibernate Validator discovers a constraint's validated type by resolving the type variable in
`ConstraintValidator<A, T>` across the validator's class hierarchy. For
`MaxValidatorForBigDecimal extends AbstractMaxValidator<BigDecimal> implements ConstraintValidator<Max, T>`,
it must match the `T` that appears in `ConstraintValidator<Max, T>` (obtained via
`AbstractMaxValidator.getGenericInterfaces()`) against the `T` declared by
`AbstractMaxValidator.getTypeParameters()`, then substitute `BigDecimal`.

CratonVM fabricates a fresh synthetic `java.lang.reflect.TypeVariable` object for each
occurrence. The two `T`s therefore are **distinct objects**, and CratonVM's synthetic
`TypeVariable` had **no `equals`/`hashCode`**, so it fell back to `Object` identity:

| probe (`MaxValidatorForBigDecimal`) | HotSpot | CratonVM (before) |
|-------------------------------------|---------|-------------------|
| `tvIface.getName()` / `tvParam.getName()` | `T` / `T` | `T` / `T` |
| `getGenericDeclaration()` (both) | `AbstractMaxValidator` | `AbstractMaxValidator` |
| `tvIface.equals(tvParam)` | **true** | **false** |
| `hashCode()` (both `T`) | `668386740 == 668386740` | **85 ≠ 29** |

Because the two `T`s compare unequal, HV's resolution fails to bind `T → BigDecimal`, the validated
type is never discovered, and no validator matches → `HV000030`. (On HotSpot the two `T`s are even
the *same* cached object, so identity already holds; CratonVM does not canonicalize, so correct
`equals`/`hashCode` is mandatory.)

## Fix (root cause: dual TypeVariable representation)

The real defect is that CratonVM was **inconsistent**: `Class.getTypeParameters()` returns the
**real** `sun.reflect.generics.reflectiveObjects.TypeVariableImpl`, while a type-variable *use* in a
parameterized type (`getGenericInterfaces()` → `ConstraintValidator<Max, T>`) was converted to a
CratonVM **synthetic** `java.lang.reflect.TypeVariable`. The two are different classes and can never
compare equal (each side's `equals` checks the other's exact class), so the substitution `T →
BigDecimal` failed (`HV000030`), and a `.equals`-only band-aid then made *every* validator's `T`
collapse to the same variable (`HV000150 multiple validators`).

**Primary fix** (`native-builtins/src/generics.rs`, `type_sig_to_java` / `TypeSig::TypeVar`): resolve
a type-variable *use* to the **real** type-parameter object declared by the enclosing generic
declaration — `resolve_declared_type_variable(decl, name)` calls `decl.getTypeParameters()` (which
already returns real `TypeVariableImpl`s) and returns the matching-named one. The use is then
**identity-equal** to what `getTypeParameters()` hands out, exactly as on HotSpot, so any
hierarchy-walking resolver substitutes correctly. Falls back to the synthetic stand-in only when the
declaration in scope declares no such parameter. The declaring class is already set as the
`GENERIC_DECL_SCOPE` by `native_class_get_generic_interfaces` ("type-variable uses in an interface
type refer to THIS class's type parameters").

**Secondary safety net** (`native-builtins/src/lang_reflect.rs`): native `equals`/`hashCode` for
`java/lang/reflect/TypeVariable` implementing the JDK `TypeVariableImpl` contract (equal iff same
generic declaration + name; `hashCode = identityHash(decl) ^ javaStringHash(name)`), handling any
residual synthetic stand-ins compared against a real `TypeVariableImpl`. Precedent for native
`equals`/`hashCode` on non-bytecode classes: `java/net/URL`, enums, records, boxed types.

After the fix `GvProbe3` reports both `T`s as `sun.reflect…TypeVariableImpl`, `declIdentitySame=true`,
`equals=true` — and `BeanValidationAutoTest` passes (`ok=1`).

## Impact

Fixes all Bean Validation constraint-resolution failures, and more generally any library that
resolves type variables across a class hierarchy via reflection (the JDK contract was simply
unimplemented for CratonVM's synthetic type variables).

## Repro

`GvProbe2` (in `.cratonvm-suite/`): loads `MaxValidatorForBigDecimal`, extracts the two `T`s, prints
`equals`/`hashCode`. Before: `equals=false`. After the fix: `equals=true`.
