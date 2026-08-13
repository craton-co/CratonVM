# JNI argument and return marshalling is wrong for most families — RESOLVED

| | |
|---|---|
| **Status** | ✅ RESOLVED 2026-08-06. All thirteen families now print byte-identical to HotSpot 25 in both modes |
| **Severity** | was high — a native returning `0` instead of `42` is silent data corruption, not a crash |
| **Modes** | BOTH. `--real-jdk` and `--jdk-only` were identical, and both are fixed |
| **Opened** | 2026-08-05, by `probes/JdkOnlyPlatformProbe`'s `jni` section (L8, criterion 6) |
| **Closed** | 2026-08-06 — seven independent causes; the one the record guessed accounted for a single row |

## The measurement, before and after

One shared object (`probes/jdkonly_jni_probe.c`), one set of class files, three
arms, `scripts/jdk-only-strict-probes.sh`. HotSpot 25 binds from the same `.so`.

| family | HotSpot 25 | CratonVM before | CratonVM after |
|---|---|---|---|
| `add(40,2)` → `jint` | `42` | **`0`** | `42` |
| `mulLong(0x7fffffff,3)` → `jlong` | `6442450941` | **`0`** | `6442450941` |
| `scale(1.5,4)` → `jdouble` | `6.0` | **`0.0`** | `6.0` |
| `reverse("craton")` → `jstring` | `notarc` | `notarc` | `notarc` |
| `sumInts(int[])` | `15` | `15` | `15` |
| `JNI_ABORT` leaves the array alone | `[1,2,3,4,5]` | `[1,2,3,4,5]` | `[1,2,3,4,5]` |
| `doubleInts(int[])` commit-back | `[2,4,6,8,10]` | **`[1,2,3,4,5]`** | `[2,4,6,8,10]` |
| `joinStrings(String[])` | `a\|b\|c` | **`null`** | `a\|b\|c` |
| `readValue` (`GetIntField`) | `7` | `7` | `7` |
| `writeValue` (`SetIntField`) | `21` | **`7`** | `21` |
| `callBackTriple(9)` (`CallStaticIntMethod`) | `27` | **`-1`** | `27` |
| `throwIse` (`ThrowNew`) | `ISE:from-native` | **both branches** | `ISE:from-native` |
| `registeredNative(5)` (`RegisterNatives`) | `true` | **`false`** | `true` |

One family was **added** while closing this record, because fixing the up-call
made its other half reachable and nothing measured it: `upcallThrow` calls a
Java method that throws, and reports what the native saw afterwards.

| family | HotSpot 25 | CratonVM before | CratonVM after |
|---|---|---|---|
| `upcallThrow(4)` (up-call throws, native clears) | `iae` | **exception escaped** | `iae` |

The whole line, after:

```
jni mapped=libcratonjniprobe.so add=42 mul=6442450941 scale=6.0 rev=notarc
    arrSum=15 abortKept=[1, 2, 3, 4, 5] doubled=[2, 4, 6, 8, 10] join=a|b|c
    field=7->21 upcall=27 upcallThrow=iae throw=ISE:from-native registered=true
```

Two consecutive full three-arm runs report
`JdkOnlyPlatformProbe/real/jni` and `JdkOnlyPlatformProbe/strict/jni` under
**NO LONGER DIVERGING**, and the baseline has been re-frozen without them.

## The seven causes

The record's "one strong inference" — that a static native is handed `0` where
HotSpot hands the declaring `jclass` — was **correct but accounted for only one
row**. It also correctly warned not to write it into a fix commit until a
measurement said so. The measurement, once taken, named six more.

### 1. A safety check refused seven of the twelve natives (`jni_fn_ptr_ok`)

The dominant cause, and the reason the failures looked like a *marshalling*
problem in the first place. `jni_fn_ptr_ok` rejected any function pointer that
was not 2-byte aligned, on the stated grounds that "function pointers must be
at least 2-byte aligned on all modern architectures".

That is false on x86-64, which has no instruction-alignment requirement at all,
and gcc relies on it: `cc -shared -fPIC -O1` put `add` at `0x11f7`, `mulLong` at
`0x11ff`, `scale` at `0x120b`. Seven of the twelve exported entry points, plus
the static `registered_impl` that `JNI_OnLoad` registers, landed on odd
addresses. Each was refused and returned `0` — so `add(40, 2)` read as `0` in
Java, from a correctly resolved pointer into correctly compiled code, because a
guard invented a rule the ISA does not have.

The five that worked (`reverse`, `sumInts`, `readValue`, `throwIse`) were the
five that happened to be even. That is the whole pattern the table showed.

aarch64 keeps its check: A64 instructions are fixed-width and genuinely must be
4-byte aligned.

### 2. Ten JNI table slots were off by one (slots 19–28)

A native never *names* a JNI function: it loads slot N from the `JNIEnv` table
and calls it. A second `GetObjectRefType` had been wired at slot 19 — its real
slot, 232, was also wired — and that pushed `PushLocalFrame` through
`AllocObject` one slot along. So:

* `DeleteLocalRef` ran `DeleteGlobalRef`
* `IsSameObject` — the idiom every library uses for `o == null` and for
  reference identity — ran the `void` `DeleteLocalRef` and returned whatever
  was left in RAX
* `NewGlobalRef` ran `PopLocalFrame`, popping a frame the VM still believed live
* `DeleteGlobalRef` ran `NewGlobalRef`, so every release leaked a global root
* `AllocObject` ran `EnsureLocalCapacity`

Every test that existed was written against the slot the table *happened* to
use rather than the slot a compiled native loads, so all of them stayed green
through it. The replacement, `jni_function_table_matches_the_jni_h_layout`,
is written the other way round: the left column is `jni.h` verbatim and
position IS the index, and it also asserts the wired function's own name
matches the header's. All 232 slots from 4 (`GetVersion`) to 235
(`GetStringUTFLengthAsLong`) now agree.

`JNI_FUNCTION_COUNT` was 234 while JDK 25's `jni.h` ends at slot 235, so
`(*env)->IsVirtualThread(env, o)` read one word past the allocation. The table
is 236 wide now, with `IsVirtualThread` and `GetStringUTFLengthAsLong` wired
rather than absent.

### 3. Static natives were handed `0` instead of the declaring `jclass`

`vm_exec`'s two JNI function-pointer dispatch arms built every call as
`(env, receiver, args…)` with `receiver = 0` when `is_static`. HotSpot passes
the declaring class, which is what `GetStaticMethodID(env, cls, …)` and
everything else written against the second parameter needs. `callBackTriple`
returned its own "GetStaticMethodID returned NULL" sentinel (`-1`). The
receiver is now `declaring_class_id.as_u32()`, the same raw-`ClassId` encoding
`FindClass` returns and the rest of the JNI table decodes.

### 4. The bare-varargs call forms were unimplemented, and the `va_list` forms were wrong on Linux

`CallStaticIntMethod(env, cls, mid, n)` is the `...` form, slot 129, which was
wired to a stub that raises `UnsatisfiedLinkError`. Defining a C-variadic
function is still unstable in Rust (rust-lang #44930) — but a variadic callee's
only extra obligation is to materialise the `va_list` its ABI describes, and
the `...MethodV` implementations were already there. All 31 `...` slots
(`NewObject` plus three groups of ten) now go through a small assembly
trampoline that builds the platform `va_list` and hands off to the `V` sibling.
Non-x86-64 keeps the loud refusal.

Underneath, `va_list_to_jvalues` read every `va_list` as a flat array of 8-byte
slots. That is right on Windows x64 and Apple arm64 and **wrong on System V**,
where a `va_list` is a four-field struct — `gp_offset`, `fp_offset`,
`overflow_arg_area`, `reg_save_area` — whose integer and SSE cursors advance
independently; reading it as an array decodes its own offset header as argument
one. So `CallXxxMethodV`, reachable directly from any native, was already
broken on Linux before the `...` forms existed. `VaCursor` now walks whichever
shape the target ABI actually uses. The same loop also read a `jfloat` argument
as raw 32 bits, when C's default argument promotions mean it arrives as a
double.

### 5. A pending exception was delivered late, not at the native's return

`ThrowNew` does not unwind: it records a pending throwable and lets the native
run to its `return`, at which point the exception becomes real to Java. Only
the Rust-registry dispatch path (`safe_native_call`) drained that slot; the two
JNI function-pointer paths returned the native's value and left the throwable
in thread-local storage for whatever ran next.

The probe caught the exact shape, because it records each branch separately
rather than printing one verdict:

```
upcall=-1 throw=<no-throw> throw=ISE:from-native registered=false
```

BOTH branches printed. The `try` body completed — so the native "returned
normally" — and the `catch` also ran, from the same `IllegalStateException`
delivered later. That is a different defect from "the exception was lost", and
worse: every statement between the native's return and the eventual delivery
executed. `jni_pending_exception_after_native` now drains the slot at the
return edge of both paths, mirroring `safe_native_call`'s handling including
its `u64::MAX` sentinel and its preference for the GC-remapped
`native_pending_return` over the raw handle.

### 6. `ExceptionCheck` was a constant, and `ExceptionDescribe` did nothing

`jni_exception_check` was `{ JNI_FALSE }` — not a check, a constant. That is
how essentially every JNI library asks "did that up-call throw?", so a hard
`false` did not fail loudly: it made every error-handling branch in every
native library unreachable, and each caller went on to use a return value the
spec leaves undefined once an exception is pending. It reads the pending slot
now — the slot rather than the GC root, because the slot is what
`ExceptionClear` resets.

`ExceptionDescribe` was an empty function. `if (ExceptionCheck(env)) {
ExceptionDescribe(env); return -1; }` is the canonical JNI error path and is
written on the understanding that nothing is pending afterwards; leaving the
exception in place means it is delivered at the native's return to a caller
that believes it handled it. It now clears first, then prints through the
throwable's own `printStackTrace`.

Both were invisible until cause 5 was fixed: with the pending slot never
drained at the native's return, whether a native could *see* an exception
changed nothing about what happened next.

### 7. A `jboolean` return was reduced by its low BIT

`Value::Int((raw_result & 1) as i32)`. `jboolean` is a byte and Java's
`boolean` is 0 or 1, so a return does have to be normalised — but by value, the
way HotSpot's native wrapper does it (`movzbl` then `setne`). `return flags &
MASK;` is ordinary C and hands back something like `0x80`, which `& 1` reads as
**false** and HotSpot reads as true.

## What was already ruled out, and stayed ruled out

* **Not the library.** HotSpot bound and ran every family correctly from the
  same `.so` in the same gate run.
* **Not symbol resolution.** `nm -D` showed all twelve symbols. It was worth
  looking at those addresses sooner: they are also what cause 1 turns on.
* **Not string conversion or array reads.**

## Regression coverage

In `vm/src/native/jni.rs`:

* `jni_function_table_matches_the_jni_h_layout` — all 232 slots against `jni.h`,
  by index and by name.
* `jni_fn_ptr_odd_address_is_callable` — the three real odd addresses.
* `jni_varargs_trampoline_delivers_the_c_argument_list` — two trampolines built
  by the same macro the 31 real ones use, called through genuine variadic
  function-pointer types so the compiler emits a real C varargs sequence for
  the active ABI (System V's `AL` included). This is what would catch a
  Windows-ABI mistake on a Windows build.
* `sysv_va_cursor_walks_both_register_classes_then_the_overflow_area`.
* `jni_bare_varargs_slots_are_dispatched_or_refused_loudly` — replaces the old
  test that asserted the slots refuse.
* `jni_exception_check_reports_the_pending_slot` and
  `jni_exception_describe_clears_the_pending_slot`.
* `dispatch_jni_native_boolean_return_is_normalised_by_value`.

End to end, `scripts/jdk-only-strict-probes.sh` is the gate.

## Residual

Nothing measured is outstanding. Two limits are stated rather than fixed:

* The `...` trampolines are x86-64 only (System V and Windows x64). On any
  other architecture the 31 slots keep `jni_varargs_unsupported`, which raises
  `UnsatisfiedLinkError` rather than fabricating a 0/null.
* `GetModule` still returns the unnamed module for every class, unchanged.

## Reproducing (for a future regression)

```sh
JAVA_HOME=/path/to/jdk25 CV=target/release/cratonvm \
    bash scripts/jdk-only-strict-probes.sh
```

The `jni` line of
`target/jdk-only-strict-probes/logs/JdkOnlyPlatformProbe.*.norm` is the table
above. The section accumulates into a builder and prints in a `finally`, so a
family that throws does not erase the verdict on the ones before it — which is
how both the `$`-mangling fix's effect and cause 1's seven-of-twelve pattern
became visible.
