# JNA `Native.<clinit>` NPE on `nativeVersion.split(...)` — FIXED (JNI `FindClass` could not return `java.lang.Object`)

**Status:** FIXED (2026-08-12). Filed the same day while standing up a
real-Postgres run of the hibernate-reactive suite on Azure host
`azureuser@20.80.105.49`, where it broke Testcontainers' rootless-Docker probe.

The symptom named JNA. The defect was that **JNI `FindClass` could not return
`java.lang.Object`** — so every JNI library whose `JNI_OnLoad` starts by caching
that class was dead on this VM.

## What it looked like

```
java.lang.NullPointerException: Cannot invoke "String.split(String)" because "nativeVersion" is null
	at com.sun.jna.Native.isCompatibleVersion(Native.java:199)
	at com.sun.jna.Native.<clinit>(Native.java:223)
```

Reached whenever Testcontainers probes `RootlessDockerClientProviderStrategy$LibC`
(a JNA `Library` interface), which forces `com.sun.jna.Native`'s static
initializer. The class loads and initializes cleanly under stock HotSpot with the
identical classpath.

The line that actually mattered was three lines earlier in the log, on stderr
from JNA's own C code:

```
JNA: Problems loading core IDs: java.lang.Object
```

`libjnidispatch`'s `JNI_OnLoad` caches a handful of core class/method ids before
doing anything else and reports the first one it cannot get. It could not get
`java.lang.Object`. Having failed, it never cached `classString` /
`MID_String_init`, so every `jstring` it later constructed was NULL — including
the return value of the `getNativeVersion()` native that `Native.<clinit>` reads.
`nativeVersion == null` was the fourth-order symptom.

## Root cause

A `jclass` in this VM's JNI table **is** a `ClassId`: `FindClass` returned
`class_id.as_u32() as JClass`, and twenty-one consumers decode it with
`ClassId::new(clazz as u32)`.

But `jclass` is a `jobject`, and in JNI a NULL `jobject` means **failure**. So
whichever class held `ClassId(0)` was unreachable through `FindClass` — the call
returned `0` and every caller read it as "no such class".

`java.lang.Object` is the first class the VM loads. `ClassId(0)` is
`java.lang.Object`: the one class essentially every JNI library asks for first,
and the only one that could never be found.

Measured with a C library that calls each JNI primitive in turn and prints the
result, loaded via `System.load` and probed from both `JNI_OnLoad` and a
registered native:

```
                                  HotSpot 25          CratonVM (before)
FindClass(java/lang/Object)  =    0x77d7840dedd0      (nil)
```

Identical from both contexts, so it was not a missing TLS context or an
`JNI_OnLoad`-specific path — `FindClass` simply could not name that class.

## Fixes

1. **`jclass` handles carry a tag bit (`JCLASS_TAG = 1 << 32`)** so the encoding
   can never collide with JNI NULL. A HIGH bit rather than a `+1` bias on
   purpose: the low 32 bits stay exactly the `ClassId`, so all twenty-one
   existing `ClassId::new(clazz as u32)` decode sites keep working untouched —
   the truncation discards the tag. A bias would have required every one of them
   to change in lockstep, and a single missed site would have silently decoded
   the WRONG class instead of failing. Producers (`FindClass`, `GetSuperclass`,
   `GetObjectClass`, `DefineClass`) go through one `class_id_to_jclass` helper.

2. **`NewGlobalRef` / `NewWeakGlobalRef` round-trip a `jclass`.** Both resolved
   their argument as an object handle, which is the one convention `FindClass`
   never produces, so both returned 0 —
   `NewGlobalRef(FindClass(env, "..."))`, the opening move of almost every
   `JNI_OnLoad`, was dead independently of the NULL collision above. A class is
   permanently live here, so a global ref to one is the same handle back;
   `DeleteGlobalRef`/`DeleteWeakGlobalRef` no-op on it rather than disturbing the
   ref table. `NewLocalRef` likewise.

3. **`IsSameObject` compares class handles directly.** Neither side resolved
   through `jobject_to_obj`, so both fell into the `(None, None) => JNI_TRUE`
   arm and **every pair of distinct classes compared equal**. Class handles are
   canonical, so a direct comparison is both correct and cheap.

4. **`GetObjectRefType`** reports `JNILocalRefType` for a `jclass` instead of
   deciding global-vs-local from the low bit of the ClassId.

5. **A regression test** (`jclass_encoding_is_never_null_and_survives_the_u32_decode`)
   pins both halves of the contract: no `ClassId` — `0` included — may encode to
   a NULL `jclass`, and the tag must vanish under the `clazz as u32` decode every
   consumer uses.

## Impact beyond JNA

This was not a JNA bug and the blast radius was not Testcontainers. Any native
library using the standard `JNI_OnLoad` idiom — `FindClass` a core class, cache
it with `NewGlobalRef`, then `GetMethodID` off the cached handle — got NULL at
step one or step two. Libraries fail that differently depending on how they
handle it, which is why it surfaced as an NPE about a version string rather than
as anything mentioning JNI.

The `DOCKER_HOST=unix:///var/run/docker.sock` workaround from the original
filing is no longer required; it only ever routed around the JNA probe, and any
code path that goes through a JNA `Library` initialization with no env override
available would still have hit it.

## Related

- `vertx-pg-sasl-scram-handshake-fails-20260812-FIXED.md` — the other, dominant
  defect from the same investigation, fixed alongside this one.
- `cassandra-jni-thrownew-discards-payload-hang-FIXED.md` — an earlier defect in
  the same JNI table, same shape of consequence.
