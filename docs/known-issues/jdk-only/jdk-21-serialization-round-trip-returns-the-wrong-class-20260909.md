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
