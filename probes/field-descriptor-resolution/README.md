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

`B` and `C` each exist in two versions, in `v1/`+`v2/` and `c1/`+`c2/`. They
declare the same public class name, so they must live in same-named files in
separate directories and be compiled separately — that separation IS the shape
under test.

```bash
JDK=/path/to/jdk-25
CV=/path/to/cratonvm

# --- the descriptor picks the RIGHT field (corrections) --------------------
"$JDK/bin/javac" -d v1c A.java v1/B.java
"$JDK/bin/javac" -cp v1c -d mainc Main.java
"$JDK/bin/javac" -cp v1c -d v2c v2/B.java && cp v1c/A.class v2c/A.class
"$JDK/bin/java" -cp "mainc:v2c" Main                       # oracle: A.x-object
"$CV" --java-home "$JDK" -cp "mainc:v2c" Main              # must match

# --- an absent (name, descriptor) pair is NoSuchFieldError (strict) --------
"$JDK/bin/javac" -d c1c c1/C.java
"$JDK/bin/javac" -cp c1c -d smainc StrictMain.java
"$JDK/bin/javac" -d c2c c2/C.java
"$JDK/bin/java" -cp "smainc:c2c" StrictMain                # oracle: NoSuchFieldError
"$CV" --java-home "$JDK" -cp "smainc:c2c" StrictMain       # must match

# the pre-2026-08-28 answer, for a single-binary A/B
CRATONVM_FIELD_RESOLUTION_NAME_ONLY=1 "$CV" --java-home "$JDK" -cp "smainc:c2c" StrictMain
```

Both print `PROBE-PASS` or `PROBE-FAIL`.
