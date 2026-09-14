# Core reflection has no member filter, and the native that should install one is a no-op

| | |
|---|---|
| **Status** | Open. Measured 2026-09-11, not fixed. |
| **Found by** | lane 5's `sun.misc.Unsafe` workload — it was the one row of 36 where the two VMs disagreed |
| **Owner** | **not lane 5.** This is core reflection and it is cross-cutting; the lane that owns `Class.getDeclaredFields`/`getDeclaredMethods` should take it |
| **Instrument** | [`apps/probes/ReflectMemberFilter.java`](../../../apps/probes/ReflectMemberFilter.java) |

## What the JDK does and CratonVM does not

`jdk.internal.reflect.Reflection` keeps two process-global maps, `fieldFilterMap`
and `methodFilterMap`, and `Class.getDeclaredFields()` / `getDeclaredMethods()`
subtract them before answering. A filtered member is not *inaccessible*, it is
**invisible**: `getDeclaredField` throws `NoSuchFieldException` for a field the
class file plainly declares.

CratonVM filters nothing. `Reflection.registerFieldsToFilter` and
`registerMethodsToFilter` are both registered as
`|_ctx, _args| Ok(None)` — accept and discard — under a comment that says:

> Our reflection layer already filters these through its own mechanism, so the
> native is a no-op that just accepts + discards the arguments.

**There is no such mechanism.** The comment is the only thing asserting it, and
nothing in the tree greps to a filter list, a `is_filtered` predicate, or a
hidden-member set.

## The measurement

`ReflectMemberFilter`, 18 rows, JDK 25.0.4+7 against a `--jdk-only` CratonVM
built from `dev` at `8f1666414`:

```text
  member                                          HotSpot     CratonVM
  M  sun.misc.Unsafe.getUnsafe                    filtered    VISIBLE
  F  java.lang.Class.classLoader                  filtered    VISIBLE
  F  java.lang.Class.classData                    filtered    VISIBLE
  F  java.lang.System.security                    filtered    filtered   <- see below
  F  jdk.internal.reflect.Reflection.fieldFilterMap   filtered    VISIBLE
  F  jdk.internal.reflect.Reflection.methodFilterMap  filtered    VISIBLE
  F  java.lang.invoke.MethodHandles$Lookup.allowedModes  filtered VISIBLE
  F  java.lang.invoke.MethodHandles$Lookup.lookupClass   filtered VISIBLE
  F  java.lang.ClassLoader.classes                filtered    VISIBLE
  F  java.lang.Module.loader                      filtered    VISIBLE
  F  java.lang.String.value          (CONTROL)    VISIBLE     VISIBLE
  M  java.lang.String.length         (CONTROL)    VISIBLE     VISIBLE
```

Nine of ten filtered members are visible here. **The two controls are the point
of the table**: `String.value` and `String.length` are visible on both, so
`getDeclaredField`/`getDeclaredMethod` work — this VM has simply never been
asked to hide anything. Without those two rows a VM whose `getDeclaredField`
threw for everything would have scored a perfect match.

`System.security` is a FALSE agreement and is marked so deliberately: that field
does not exist in this VM's `System` at all, so "not found" is the right answer
for the wrong reason. A row that agrees because the member is missing is not
evidence of a filter.

## The counts say the gap is wider than the named rows

```text
  class                                  HotSpot          CratonVM
  sun.misc.Unsafe                        97 methods       98 methods
  java.lang.Class                        21 fields        26 fields
  jdk.internal.reflect.Reflection         0 fields         4 fields
  java.lang.invoke.MethodHandles$Lookup  17 fields        19 fields
  java.lang.ClassLoader                   0 fields        18 fields
```

`ClassLoader` is the loud one: HotSpot filters **every** field of it and this VM
exposes eighteen. A fix that satisfies only the ten named rows above would leave
all five counts apart, which is why the counts are in the probe.

## Why it matters

* **Any library that walks a JDK class's members sees members HotSpot hides.**
  Serialization frameworks, mocking libraries, DI containers and dumpers all do
  this. `java.lang.Class` alone reports five fields that should not be there,
  and a walker that reflects over them gets VM-internal state.
* **`MethodHandles$Lookup.allowedModes` and `lookupClass` are reflectable.**
  Those two carry a `Lookup`'s access rights. The JDK filters them precisely so
  that a `Lookup` cannot be re-pointed or widened by reflection, and the filter
  is the whole enforcement — there is no second check behind it.
* **`sun.misc.Unsafe.getUnsafe` is reachable by reflection**, which is the door
  JEP-era hardening closed. CratonVM's own `getUnsafe` native is CORRECT — it
  throws `SecurityException` for a caller off the boot path, measured against
  HotSpot — but on HotSpot you cannot get far enough to be refused, because the
  method is not in the reflective surface at all.

## Do NOT read a row against `javap`

`javap -p sun.misc.Unsafe` prints `public static sun.misc.Unsafe getUnsafe();`
on the **17, 21 and 25** images alike. The member is declared, is public, and is
still invisible to core reflection at runtime. The image is not the authority on
what reflection answers — which is why this is a runtime probe and why a census
built from class files cannot see this defect at all.

That trap cost a wrong claim before this page existed: lane 5's residual wave
first recorded `getUnsafe` as "absent from the JDK 25 image, therefore a
bucket-F deletion", on the strength of a `NoSuchMethodException` from
`getMethod`. It is neither absent nor a deletion candidate; the exception was
the filter working.

## What a fix looks like

1. Give the two `register*ToFilter` natives somewhere to record — a per-class
   set of member names, process-global like the JDK's.
2. Consult it in `getDeclaredFields`/`getDeclaredMethods` and in the
   single-member `getDeclaredField`/`getDeclaredMethod` lookups, which must
   agree; the probe asks through the single-member form and the counts through
   the array form on purpose.
3. Seed it the way the JDK does — from `Reflection`'s own static initialiser
   running in the image — rather than from a hand-written list in Rust. A
   hand-written list is a second source of truth for a set the image already
   carries, and it will drift the first time a JDK version changes an entry.
   If the initialiser cannot run, the hand-written list needs a version-pinned
   test, because this set IS version-pinned.

The probe is the acceptance test: 18 rows, and `filtered` is the expected answer
for every row except the two controls.
