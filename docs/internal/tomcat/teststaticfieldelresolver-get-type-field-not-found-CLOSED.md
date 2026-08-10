# `TestStaticFieldELResolver` — `canAccess` refused a public member of a package-private class

| | |
|---|---|
| **Status** | ✅ **CLOSED 2026-08-10** — root-caused and fixed in `fix/tomcat-elstatic-naming-20260810` |
| **HotSpot** | PASS (`OK (34)`) |
| **CratonVM** | was FAIL 2/34 on all 3 GC backends; now `OK (34)` on default / G1 / ZGC |
| **Fix** | `verify_member_access` (`native-builtins/src/lang_class.rs`) |
| **Discovered** | 2026-08-10, cross-referencing FAILs common to all three GC-backend full-suite runs |

## Symptom

```
1) testGetValue09(jakarta.el.TestStaticFieldELResolver)
jakarta.el.PropertyNotFoundException: No public static field named [GET_TYPE]
was found on exported class [jakarta.el.TestStaticFieldELResolver$MethodUnderTest]
	at jakarta.el.StaticFieldELResolver.getValue(StaticFieldELResolver.java:62)
2) testGetType09  (same shape, via StaticFieldELResolver.getType)
```

**The message named the wrong thing, and so did this page's first draft.** There
is no second field: `MethodUnderTest.GET_TYPE.toString()` is `"GET_TYPE"`, and
the field it resolves is the *enum constant itself* — `public static final`, as
javac emits every enum constant. The original page guessed at "a static field
whose simple name collides with an enum constant's name … getting deduplicated,
shadowed, or dropped somewhere in CratonVM's field-table construction". Nothing
was dropped. `getDeclaredFields()`, `getFields()` and `getField("GET_TYPE")` all
answered exactly what HotSpot answers, modifiers included.

## Root cause

`StaticFieldELResolver.getValue` guards its read with three tests:

```java
Field field = clazz.getField(name);
int modifiers = field.getModifiers();
if (Modifier.isStatic(modifiers) && Modifier.isPublic(modifiers) && Util.canAccess(null, field)) {
    return field.get(null);
}
```

The third one — `Util.canAccess` → `AccessibleObject.canAccess` → CratonVM's
`verify_member_access` — was the only one that disagreed with HotSpot:

| `jakarta.el.ELStaticProbe$MethodUnderTest.GET_TYPE` | HotSpot | before | after |
|---|---|---|---|
| `clazz.getField("GET_TYPE")` | found | found | found |
| `field.getModifiers()` | `0x4019` | `0x4019` | `0x4019` |
| `field.canAccess(null)` | `true` | **`false`** | `true` |
| `field.get(null)` | `GET_TYPE` | `GET_TYPE` | `GET_TYPE` |

`verify_member_access` asked the class-accessibility question **only inside its
public-member arm**, and answered it as *"is the declaring class public?"*:

```rust
if (modifiers & SA_ACC_PUBLIC) != 0 {
    return (i32::from(ctx.class_access_flags(declaring_id)) & SA_ACC_PUBLIC) != 0;
}
```

`Reflection.verifyMemberAccess` asks it *before* any member modifier and phrases
it as *"is the declaring class public **OR** is the caller in its runtime
package?"*. `MethodUnderTest` is a **private nested enum**, so its class-file
access flags carry no `ACC_PUBLIC` — and the caller, `jakarta.el.Util`, is in the
declaring class's own package. HotSpot allows it; the old arm refused it without
ever looking at the package.

**`Field.get` was already right.** It goes through a *different* helper,
`public_member_class_is_reachable`, which does test the package. So the two gates
disagreed, and only the `canAccess` one was wrong. That is why the exception
carried a **null cause**: no reflective exception was ever thrown, the resolver
simply fell off the end of its `if` and built the "not found" message itself. An
error whose text says "no such field" while every field API can see the field is
the signature to remember here.

## The fix

`verify_member_access` now mirrors `Reflection.verifyMemberAccess`'s order: the
class half first, for every member kind, then the modifier half.

```rust
let same_package = /* caller's package == declaring class's package */;
if (class_access_flags(declaring_id) & ACC_PUBLIC) == 0 && !same_package {
    return false;                      // class itself unreachable
}
if public   { return true; }           // class reachable + public member
if private  { return are_nestmates(caller, declaring); }   // JEP 181
if same_package { return true; }
/* protected + subclass, then the JLS 6.6.2.1 receiver narrowing, unchanged */
```

Two smaller divergences close with it, both measured by
`NestAccessProbe` (all rows now match HotSpot):

* a **protected** member of a package-private class was reachable from a
  foreign-package subclass — HotSpot refuses it at the *class* level, before the
  protected rule is reached;
* a **private** member was refused from a **nestmate**, so an outer class could
  not read its nested class's private members without `setAccessible`. JEP 181
  allows it and `Reflection.verifyMemberAccess` runs `areNestMates` for exactly
  this case.

`are_nestmates` compares nest-host **names** plus the defining loader, never a
resolved `ClassId`: `nest_host_name` answers with the `NestHost` attribute's
name (`None` meaning "I am my own host"), and resolving that name back to an id
is a loader-ambiguous step this does not need to take. JVMS §5.4.4 already
requires a nest member to share its host's run-time package, so name + loader is
sufficient — and requiring the loader keeps two same-named classes under
different loaders from being conflated into one nest.

## Verification

Baseline binary `cratonvm-elnaming-base-20260810.exe` (dev @ `845a1ca75`) vs
`cratonvm-elnaming-fix1-20260810.exe` (same tree + the fix), through
`apps/tomcat-suite-runner/run-one.ps1` so the launch environment is the suite's:

| | default GC | `-XX:+UseG1GC` | `-XX:+UseZGC` |
|---|---|---|---|
| before | FAIL 2/34 | FAIL 2/34 | FAIL 2/34 |
| after | `OK (34)` | `OK (34)` | `OK (34)` |
| HotSpot control | `OK (34)` | — | — |

Probes used, both reusable:

* `apps/tomcat/.suite/probe-el/jakarta/el/ELStaticProbe.java` — replays the
  resolver's exact lookup inside package `jakarta.el` and prints every
  intermediate decision instead of throwing. This is what separated "the field
  is missing" from "the field is there and the access gate refused it"; the
  original page's suspicion could not have been refuted from the failure text
  alone.
* `NestAccessProbe` — the eight `canAccess` rows the rewritten function
  decides, against a HotSpot control.

## Regression pin

`regression-suite/src/RCanAccessRules.java` (+ its foreign-package half
`RCanAccessOutsider.java`), registered in `CORE_CLASSES`. Sixteen `canAccess`
rows across every arm of JLS 6.6.1, including the enum-constant shape this page
is about, and — importantly — the two rows that must stay `false`: the same
public member of the same package-private class, asked from a class in a
different package. Without those, the widening this fix makes would read green.

RED-then-GREEN, verified, not assumed:

| binary | `RCanAccessRules` |
|---|---|
| `cratonvm-elnaming-base-20260810.exe` (dev, pre-fix) | **FAIL** (`AssertionError`) |
| `cratonvm-elnaming-fix1-20260810.exe` | PASS, output byte-identical to HotSpot |

Only booleans are printed. A `Field.toString()` or an identity hash would make
the runner's HotSpot output diff fail for a correct VM.

## Known limitation left in place

The same-package test compares package **names** only, not
`Reflection.isSameClassPackage`'s name-plus-defining-loader pair. That is a
deliberate match to the sibling gate `public_member_class_is_reachable`, which
`Field.get` asks and which has always compared names alone: the two gates
disagreeing is exactly the defect above, so they are kept in step. Two classes
sharing a package name under *different* loaders are distinct run-time packages
to HotSpot and would be refused there. Tightening both gates together needs its
own witness, and no test in the tree currently produces one.

## Adjacent finding, deliberately NOT fixed here

`canAccess` on a **non-static** member with a `null` receiver returns `false`
where HotSpot throws `IllegalArgumentException("null object for …")` before any
access check (`Integer.class.getDeclaredField("value").canAccess(null)`). It
lives in `can_access_member` (`native-builtins/src/lang_reflect.rs`), not in the
function this page fixed, and it changes no EL behaviour — `jakarta.el.Util`
catches `IllegalArgumentException` and returns `false` either way, so the Tomcat
suite cannot witness it. Left out to keep this branch's blast radius on the two
docs it retires; a fix needs a different witness to prove its RED.
