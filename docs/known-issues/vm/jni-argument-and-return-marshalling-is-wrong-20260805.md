# JNI argument and return marshalling is wrong for most families

| | |
|---|---|
| **Status** | OPEN — newly VISIBLE, not new. Two binding defects were fixed first; nothing below could be reached before that |
| **Severity** | high — a native returning `0` instead of `42` is silent data corruption, not a crash |
| **Modes** | BOTH. `--real-jdk` and `--jdk-only` are identical |
| **Opened** | 2026-08-05, by `probes/JdkOnlyPlatformProbe`'s `jni` section (L8, criterion 6) |

## Why this record exists only now

Until 2026-08-05 no user JNI library could bind a single method, for two
independent reasons, both fixed:

* `jni_encode` never escaped `$`, so the VM looked for
  `Java_JdkOnlyPlatformProbe$JniProbe_add` while the compiler emits
  `Java_JdkOnlyPlatformProbe_00024JniProbe_add`. Every native on a nested class
  — which is the conventional shape — failed to resolve.
* `RegisterNatives` decoded its `JClass` through `jobject_to_obj` while
  `FindClass` returns a raw `ClassId` and the rest of the JNI table decodes it
  as one. `class_name` was always `None` and the function always returned
  `JNI_ERR`, so the `FindClass` + `RegisterNatives` idiom every `JNI_OnLoad` is
  built on registered nothing.

With both fixed the natives are reached, and the marshalling underneath is
what this record is about. **Nothing here is a regression**: these paths had no
observable behaviour at all before, because nothing could call them.

## The measurement

One shared object (`probes/jdkonly_jni_probe.c`, built by
`scripts/jdk-only-strict-probes.sh`), one set of class files, three arms.
HotSpot 25 binds from the same `.so`.

| family | HotSpot 25 | CratonVM | verdict |
|---|---|---|---|
| `add(40,2)` → `jint` | `42` | **`0`** | wrong |
| `mulLong(0x7fffffff,3)` → `jlong` | `6442450941` | **`0`** | wrong |
| `scale(1.5,4)` → `jdouble` | `6.0` | **`0.0`** | wrong |
| `reverse("craton")` → `jstring` | `notarc` | `notarc` | OK |
| `sumInts(int[])` | `15` | `15` | OK |
| `JNI_ABORT` leaves the array alone | `[1,2,3,4,5]` | `[1,2,3,4,5]` | OK |
| `doubleInts(int[])` commit-back (mode 0) | `[2,4,6,8,10]` | **`[1,2,3,4,5]`** | wrong |
| `joinStrings(String[])` | `a\|b\|c` | **`null`** | wrong |
| `readValue` (`GetIntField`) | `7` | `7` | OK |
| `writeValue` (`SetIntField`) | `21` | **`7`** | wrong |
| `callBackTriple(9)` (`CallStaticIntMethod`) | `27` | **`-1`** | wrong |
| `throwIse` (`ThrowNew`) | `ISE:from-native` | see below | wrong |
| `registeredNative(5)` (bound by `RegisterNatives`) | `true` | **`false`** | wrong |

The `throwIse` line is the strangest and worth quoting verbatim, because the
probe prints one branch per outcome and CratonVM printed **both**:

```
... upcall=-1 throw=<no-throw> throw=ISE:from-native registered=false
```

`throw=<no-throw>` is the try body's last statement; `throw=ISE:` is the catch.
Both ran, which means the `IllegalStateException` was delivered *after* the
native returned normally rather than at its return — a deferred pending
exception. That is a different failure from "the exception was lost", and the
probe only distinguishes them because it records each branch separately.

## The one strong inference

`callBackTriple` is `GetStaticMethodID(env, cls, "triple", "(I)I")` followed by
`CallStaticIntMethod`, and it returns `-1` — the C code's "GetStaticMethodID
returned NULL" path. So the **`cls` argument the VM passes to a static native
is not a usable `jclass`**.

`dispatch_jni_native` (`vm/src/native/jni.rs`) builds every call as
`(env, receiver, args…)` and sets `receiver = 0u64` when `is_static`. HotSpot
passes the declaring class there. That single gap explains `callBackTriple`
directly, and is the first thing to check for the rest — but it does NOT
obviously explain `add` returning `0`, so treat it as the starting point, not
the diagnosis. Do not write "the static jclass" into a fix commit until a
measurement says so; this repo's own record on that is in
`docs/feature-designs/jdk-only-wave2/README.md` under "a number in a record is
a claim, not a measurement".

## What is already ruled out

* **Not the library.** HotSpot binds and runs every family correctly from the
  same `.so` in the same gate run.
* **Not symbol resolution.** `nm -D` shows all twelve symbols, and the failures
  are now wrong VALUES rather than `UnsatisfiedLinkError`.
* **Not string conversion or array reads.** `GetStringUTFChars`/`NewStringUTF`
  and `GetIntArrayElements` in read mode are byte-correct.

## Reproducing

```sh
JAVA_HOME=/path/to/jdk25 CV=target/release/cratonvm \
    bash scripts/jdk-only-strict-probes.sh
```

The `jni` line of `target/jdk-only-strict-probes/logs/JdkOnlyPlatformProbe.*.norm`
is the table above. The section accumulates into a builder and prints in a
`finally`, so a family that throws does not erase the verdict on the ones
before it — that is exactly how the `$`-mangling fix's effect became visible.
