# JDK 21: every serialization round-trip returns the wrong class -- FIXED, and the mechanism was one silently-skipped `if`

**Status:** FIXED 2026-09-09, the day after the row was frozen. The page below
is kept unedited from `docs/known-issues/jdk-only/` because its narrowing is
what made the fix findable; this header records what the open question turned
out to be.

## The mechanism: LINK 2 of a five-link chain, and every link failed silently

`native_constructor_new_instance` recognised the serialization constructor by
walking five links, each of which fell through without a word when it did not
hold:

```
1  Constructor.constructorAccessor is a non-null object
2  ...whose class is jdk/internal/reflect/DirectConstructorHandleAccessor   <-- FAILED on 21
3  ...whose field `target` is a non-null object
4  ...whose field `instanceClass` is a non-null object
5  ...which resolves to a class id
```

Link 2 is false on JDK 21, because the two images do not install the same
accessor object for the same contract. Measured directly on both images
(`RFChain2.java`, a `privateLookupIn` + `findVarHandle` walk):

```
                              JDK 21.0.12+8                              JDK 25.0.4+7
constructorAccessor           GeneratedSerializationConstructorAccessorN  DirectConstructorHandleAccessor
  its declared fields         [] -- NONE AT ALL                           [paramCount, target]
  target                      absent                                      DirectMethodHandle$Constructor
  instanceClass               absent                                      present
```

JDK 21 installs a class the JDK **generates at run time**
(`MethodAccessorGenerator`); JDK 25 installs the method-handle accessor. So on
21 there is no `instanceClass` anywhere to read, the name test simply did not
match, and control fell through to the generic path -- which allocates `clazz`,
the constructor's DECLARING class, i.e. the ancestor. That single skipped branch
is the entire defect, and it produced BOTH faces this page describes: silent
bare `Object` where the ancestor is concrete, `InvalidClassException` where it is
abstract.

**The choice is not configurable, which retires this page's own flag note.**
Neither `-Djdk.reflect.useDirectMethodHandle` (true or false) nor
`-Djdk.reflect.noInflation=true` moves either image:

```
JDK21 <default> / useDirectMethodHandle=true / =false / noInflation=true
      -> GeneratedSerializationConstructorAccessor1   (all four)
JDK25 <default> / useDirectMethodHandle=true / =false / noInflation=true
      -> DirectConstructorHandleAccessor             (all four)
```

Section 3 below recorded that the flag produced byte-identical wrong output and
concluded the flag "is not the variable". That was right, and this is why: the
flag never had anything to move.

## The fix

`native-builtins/src/lang_class.rs` -- an arm for the 21 shape that **runs the
accessor** instead of re-deriving the target type from an object that does not
carry it. The generated accessor's own bytecode is already exactly right
(`new <target>; invokespecial <ancestor>.<init>()V; areturn`), so the fix is to
stop pretending to know a JDK-internal layout that turned out to be
version-pinned, and let the real JDK bytecode be authoritative -- which is the
`--jdk-only` premise. Dispatch is `invoke_virtual_bytecode_only`; an ordinary
virtual dispatch would re-enter this same native and recurse forever.

## Verification

`SerChk.java` (part A round-trip identity, part B the factory below it, with a
`String` control that must pass even on a fully broken build). Debug binary,
Linux, both real JDK images:

```
                       BEFORE                     AFTER
--real-jdk  JDK 21     pass=1 fail=8              pass=7 fail=2
--real-jdk  JDK 25     pass=7 fail=2              pass=7 fail=2
--jdk-only  JDK 21     (same as --real-jdk)       pass=7 fail=2
--jdk-only  JDK 25     (same as --real-jdk)       pass=7 fail=2
```

All four cells are now identical: **JDK 21 behaves exactly as JDK 25 does.**
Every row this page tabulated is fixed -- the three silent `Object` rows
(`Integer`, `Long`, `Boolean`) and the two loud ones (`ArrayList`, `HashMap`).

The two residual failures are NOT this defect and were present on JDK 25 all
along, which is why the "after" column matches the 25 "before" column exactly:
see `docs/internal/retired/the-serialization-constructor-refused-by-the-module-check-FIXED-20260909.md`
(it was FIXED later the same day: `ReflectionFactory` was being skipped as
reflection plumbing, so the caller walk blamed the application class).

At the gate level, the `21-linux` strict corpus reports the whole probe clean:

```
== JdkOnlyBreadthProbe ==
transcript: byte-identical to HotSpot in both modes

NO LONGER DIVERGING (not a failure -- re-freeze to keep the ratchet tight):
  - JdkOnlyBreadthProbe/real/PROBE2
  - JdkOnlyBreadthProbe/real/SECTION-FAILED:serialization
  - JdkOnlyBreadthProbe/real/serialization
  - JdkOnlyBreadthProbe/strict/PROBE2
  - JdkOnlyBreadthProbe/strict/SECTION-FAILED:serialization
  - JdkOnlyBreadthProbe/strict/serialization
```

## Tests

`native-builtins/src/lang_class.rs::serialization_constructor_accessor_tests` --
three mock tests, deliberately NOT probe tests: this defect is invisible on a
JDK 25 image, and a JDK 25 image is what CI has. They assert the delegation
happens, that it survives the accessor name's per-class generation counter
(`...Accessor1` vs `...Accessor47` -- an equality test would have handled only
the first constructor in a process), and a control asserting an unrelated
accessor is NOT diverted into this arm.

## What this did NOT fix

The other JDK 21 rows in the `21-linux` baseline are a different defect and are
untouched: `JdkOnlyCensusLoadProbe/strict/io` fails with
`NoSuchMethodError: java.lang.System$1.encodeASCII` and the `vthreads` rows with
`NoSuchMethodError: java.lang.System$1.parkVirtualThread`. Both are the
`JavaLangAccess` carrier being pinned to `System$1`, which on JDK 21 is a
`PrivilegedAction` -- the carrier is `System$2`. That fix is parked, unmerged, on
`claude/jla-carrier-20260909`.

This page had guessed the relationship in its section 6: same species, different
mechanism. That held up. The carrier defect did not cause this one, and fixing
this one did not touch the carrier rows.

---

*(original page follows, unedited)*

# JDK 21: every serialization round-trip returns the wrong class, and only one of the two symptoms is loud

**Status:** open, root-caused at the contract level, mechanism NOT yet identified.
**Applies to:** JDK 21 only. JDK 25 is clean in the same binary.
**Modes:** BOTH `--real-jdk` and `--jdk-only`. This is a compatibility defect.
**Found:** 2026-09-09, narrowing the `JdkOnlyBreadthProbe/serialization` row that
the `21-windows` strict-corpus baseline froze on 2026-09-08.

## 1. What the corpus said, and what was actually there

The baseline froze one row:

```
SECTION-FAILED serialization: java.lang.RuntimeException:
  java.io.InvalidClassException: java.util.ArrayList; unable to create instance
```

That is one symptom of two, and it is the *less* serious one. The probe's
section aborts at its first throw, so everything behind it was invisible.
Narrowing the round-trip type by type:

| type wrote | HotSpot 21 | CratonVM 21 | CratonVM 25 |
|---|---|---|---|
| `java.lang.Integer` | `Integer` 42 | **`java.lang.Object`** | `Integer` 42 |
| `java.lang.Long` | `Long` 7 | **`java.lang.Object`** | `Long` 7 |
| `java.lang.Boolean` | `Boolean` true | **`java.lang.Object`** | `Boolean` true |
| `java.util.ArrayList` | `[a, b]` | **InvalidClassException** | `[a, b]` |
| `java.util.HashMap` | `{k=v}` | **InvalidClassException** | `{k=v}` |
| `java.lang.String` | `hello` | `hello` | `hello` |

**`Integer` deserialises to a bare `java.lang.Object` and throws nothing.** A
program gets an object back, assigns it, and fails somewhere else entirely --
or does not fail at all and writes the wrong thing down. That is worse than the
`ArrayList` row, and it is only visible because the loud row was narrowed.

`String` survives because it is handled by the stream itself
(`TC_STRING`), not by the object-construction path below.

## 2. One contract, two symptoms

Deserialising a `Serializable` class allocates the target and runs the no-arg
constructor of its **first non-serializable superclass**:

* `Integer` -> `Number` (Serializable) -> `Object`. First non-serializable
  ancestor is `Object`, which is concrete. Allocating the ancestor instead of
  the target therefore SUCCEEDS and returns a bare `Object`. Silent.
* `ArrayList` -> `AbstractList`, which is **abstract**. Allocating the ancestor
  throws `InstantiationException`, which `ObjectStreamClass.newInstance` wraps
  as `InvalidClassException(... "unable to create instance")`. Loud.

So both rows are the SAME defect. Whether it is silent or loud is decided by
nothing more than whether the ancestor happens to be abstract.

Confirmed directly, below `ObjectInputStream`, on the factory that produces the
constructor (`sun.reflect.ReflectionFactory.newConstructorForSerialization`):

```
                                              ctorDeclaredBy      newInstance ->
HotSpot 21    target=java.lang.Integer        java.lang.Object    java.lang.Integer   OK
HotSpot 21    target=java.util.ArrayList      java.util.AbstractList  java.util.ArrayList  OK
CratonVM 21   target=java.lang.Integer        java.lang.Object    java.lang.Object    WRONG
CratonVM 21   target=java.util.ArrayList      java.util.AbstractList  THREW InstantiationException
CratonVM 25   (all four)                                                              OK
```

The declaring class is right on every row. **Only what gets allocated is
wrong**, and it is wrong by being the declaring class rather than the target --
which is precisely the contract `newConstructorForSerialization` exists to
break.

## 3. What this is NOT -- both ruled out with evidence

Two obvious explanations were tested and are wrong. Recording them so the next
person does not re-spend the day.

**Not CratonVM's own `newConstructorForSerialization` native.** That native
exists (`serialization.rs::native_reflection_factory_new_constructor_for_serialization`,
which installs a `DirectConstructorHandleAccessor` whose target handle carries
`instanceClass`) but it is deliberately not registered on the real-JDK path --
`essential_path_does_not_override_reflection_factory_serialization` asserts
exactly that. Verified at runtime rather than from the test's name:
`CRATONVM_DBG_REFLECTION_FACTORY=1` prints **0** `[rf-ser]` lines on JDK 21 AND
on JDK 25. The JDK's own bytecode runs in both cases; one gets it right.

**Not a JDK-25-only internal that 21 lacks.** This was the working hypothesis --
`lang_class.rs`'s `Constructor.newInstance` recognises the serialization case by
testing whether `constructorAccessor` is a
`jdk/internal/reflect/DirectConstructorHandleAccessor` and then reading
`instanceClass` off its `target`, and its own comment predicts this defect's
exact symptom ("Allocating `clazz` here would produce a bare
`java.lang.Object`"). It is a fair suspect and it is a genuine 25-shaped pin.
But the shapes are present on BOTH images (`javap -p`, Temurin 21.0.12.1+1 vs
Microsoft 25.0.3+9):

```
                                              JDK 21     JDK 25
  Constructor.constructorAccessor             present    present
  jdk.internal.reflect.DirectConstructorHandleAccessor   present    present
  java.lang.invoke.DirectMethodHandle$Constructor        present    present
    ...its instanceClass field                present    present
```

So "the class does not exist on 21" is refuted. Something reaches that
recognition test with a different value on 21, or does not reach it at all.

**Not the accessor-implementation switch.** `-Djdk.reflect.useDirectMethodHandle`
set to `true` and to `false` produce byte-identical wrong output on JDK 21, so
the choice between the method-handle accessor and the generated-bytecode
accessor is not the variable.

## 4. Reproducing

`Ser2.java` / `RF.java` (this page's measurements) are three dozen lines; the
short form is:

```java
Object back = new ObjectInputStream(new ByteArrayInputStream(enc(42))).readObject();
System.out.println(back.getClass());   // JDK 25: java.lang.Integer
                                       // JDK 21: java.lang.Object
```

```
JAVA_HOME=<jdk21> cratonvm --real-jdk -cp . Ser2
```

Measured with a DEBUG binary built from `origin/dev` @ ccf731dd3 with an empty
`git diff origin/dev`, so no local change is in it.

## 5. Why it is worth doing next

It is not a strict-mode nicety: it is in `--real-jdk`, it hits `Integer`,
`Long`, `Boolean`, `ArrayList` and `HashMap` -- i.e. essentially every
serialized payload -- and one half of it is silent. The `21-windows` baseline
currently freezes it as one known row, which is the right call for a gate but
should not be read as "one small divergence".

## 6. Relation to the other 21 finding

Same species as the `JavaLangAccess` carrier (`System$1` on 25, `System$2` on
21): a JDK-internal detail that was verified once, on one image, and is
version-dependent. Different mechanism, and the carrier fix does not touch this
path -- but the lesson is the same one, and it is now twice in two days.
