# `getfield` must honour the constant pool's DESCRIPTOR, not just the name

JVMS §5.4.3.2 resolves a `CONSTANT_Fieldref` by name **and** descriptor. This
probe builds the ordinary binary-compatibility shape that separates the two
keys, using nothing but `javac` and separate compilation.

`Main` is compiled against a `B` that does **not** declare `x`, so `b.x` is the
inherited `A.x`, and javac records

```
getfield #10   // Field B.x:Ljava/lang/Object;
```

— the constant pool names **B**, with **A**'s descriptor. `Main` is then run
against a `B` that *does* declare `public int x = 42`, a legal separately
compiled change. Resolution has to walk past B's `int x`, because its descriptor
is not the one recorded, and land on A's `Object x`.

Matching on the name alone stops at B's `int x` and hands back its slot index.
Compiled and interpreted code then read an `int` cell as a reference.

| | `b.x` |
|---|---|
| HotSpot 25 | `A.x-object` |
| CratonVM before 2026-08-27 | **`null`** |
| CratonVM after | `A.x-object` (JIT and `--nojit`) |

The `null` is not incidental: an `int` cell read through a reference field is
the punned-cell containment path, so the defect surfaced as "this reference
cannot be null" — the same species as the two known-issue pages closed the same
day.

## The other half: the pair is absent

`StrictMain` + `Cv1.java` / `Cv2.java` build the case where the recorded
`(name, descriptor)` exists **nowhere**: compiled against a `C` declaring
`Object x`, run against a `C` whose `x` is an `int`. JVMS §5.4.3.2 makes that a
`NoSuchFieldError`.

| | result |
|---|---|
| HotSpot 25 | `NoSuchFieldError` |
| CratonVM before 2026-08-28 | resolved to `C.x:I` and read an int through a reference field |
| CratonVM after | `NoSuchFieldError` |

`CRATONVM_FIELD_RESOLUTION_NAME_ONLY=1` restores the lenient answer, for a
single-binary A/B.

## Run

```bash
JDK=/path/to/jdk-25
"$JDK/bin/javac" -d v1c A.java Bv1.java
"$JDK/bin/javac" -cp v1c -d mainc Main.java
"$JDK/bin/javac" -cp v1c -d v2c Bv2.java && cp v1c/A.class v2c/A.class
"$JDK/bin/java"      -cp "mainc:v2c" Main     # oracle
<cratonvm> --java-home "$JDK" -cp "mainc:v2c" Main

# the strict half
"$JDK/bin/javac" -d c1c Cv1.java
"$JDK/bin/javac" -cp c1c -d smainc StrictMain.java
"$JDK/bin/javac" -d c2c Cv2.java
"$JDK/bin/java"      -cp "smainc:c2c" StrictMain   # oracle
<cratonvm> --java-home "$JDK" -cp "smainc:c2c" StrictMain
CRATONVM_FIELD_RESOLUTION_NAME_ONLY=1 <cratonvm> ... StrictMain   # the old answer
```

`Bv1.java` and `Bv2.java` both declare `class B`; compile them into the separate
output directories shown, never together.
