# `String.hashCode()` on a UTF-16 string hashed the backing BYTES — FIXED 2026-08-05

**Status:** FIXED. Filed 2026-08-04 while removing the forced-native
`java/lang/String` policy; root-caused and fixed the next day.

**Reproducers:** `probes/StringUtf16HashProbe`,
`probes/StringUtf16ClassShapeProbe`, `probes/StringUtf16DispatchProbe`,
`probes/StringHashCostProbe`.

## The defect

For any `String` whose backing array is UTF-16 (any code unit `> 0xFF`),
`String.hashCode()` returned the JLS fold over **the first `length()` bytes of
the backing array, each sign-extended to a `char`** — `(char) value[i]` where
it needed `getChar(value, i)`:

```
GREEK  units=[03A3 039F 03A3]   hashCode=62956255   JLS/HotSpot=924359
```

Latin-1 was never affected, which is why it survived: only a string containing
a code unit above 0xFF reaches the broken path at all.

## Where it was

`native-builtins/src/phases_early.rs::native_arrays_support_vectorized_hash_code`
— the override for `jdk.internal.util.ArraysSupport.vectorizedHashCode`, which
modern `StringUTF16.hashCode` delegates to:

```java
public static int hashCode(byte[] value) {
    return ArraysSupport.vectorizedHashCode(value, 0, value.length >> 1, 0, T_CHAR);
}
```

The override looped **one array slot per iteration for every `BasicType`**.
`T_CHAR` is the one type whose element is *two* slots: the array is a `byte[]`,
`length` counts CHARS, and each char is a byte pair. So a 3-char string read
bytes 0, 1, 2 — and `(elem as u16)` on a signed byte from `get_array_element`
sign-extended each one.

Its own comment named the contract it was not implementing:

> `T_CHAR` → `utf16hashCode(byte[])` → big-endian u16 pairs

Two errors in one line: the pairs were never formed, and "big-endian" is not
this VM's layout either (`native_string_utf16_is_big_endian` returns false, and
every Rust accessor reads little-endian).

The fix reads `value[2i]` / `value[2i+1]`, masks both, combines little-endian,
and scales the bounds guard by the same stride. Every other `BasicType` keeps
the one-slot loop, unchanged.

### The first fix was wrong, and every probe passed anyway

The version committed as `304ee5ff5` selected the paired read on
**`basicType == T_CHAR` alone**. That is not the discriminator. `T_CHAR`
arrives with two different array types and they are not the same read:

| caller | array | slots per element | `length` counts |
|---|---|---|---|
| `StringUTF16.hashCode(byte[])` | `byte[]` | **2** | chars |
| `Arrays.hashCode(char[])` | `char[]` | **1** | chars |

So the first fix traded one broken shape for another: it corrected every UTF-16
`String` and silently broke `Arrays.hashCode(char[])`, which had been correct
all along. The discriminator is the **array**, via `ctx.heap_element_type_of`,
not the `basicType`.

What caught it was the pre-existing unit test
`t2_arrays_support_hash_code_char_zero_extends`. What did **not** catch it was
any of the nine Java probes written for this investigation, all of which passed
— none of them calls `Arrays.hashCode(char[])`. The probes were built to
interrogate `String`, so they covered the `String` half of a shared native and
were blind to the other caller by construction. A probe suite aimed at the
symptom does not cover the fix's blast radius; the callers of the function you
edited do.

Both shapes now have a test, kept adjacent with a comment saying why, and both
were verified by **injecting the exact defect** rather than by assuming the
assertions were load-bearing:

* injecting `basic_type == HOTSPOT_T_CHAR` (the shipped bug) fails
  `…_char_zero_extends` and *passes* the two new `byte[]` tests — which is the
  point: the new tests are blind to it, and the old one is not;
* injecting `false` (the original pre-fix bug) fails the `byte[]` tests.

Neither injection alone would have proved the pair. The defect never reached
`dev`.

## How it was localised, since none of the obvious answers was right

Each step ruled out a whole class of cause by measurement:

| Hypothesis | Instrument | Verdict |
|---|---|---|
| the `String` object is corrupt | `StringBackingArrayProbe` reads `value`/`coder` reflectively | **no** — `coder=1`, bytes `A3 03 9F 03 A3 03`, `charAt` matches HotSpot |
| a codegen bug | `--nojit` | **no** — bit-identical |
| a registered native | census | **no** — none for `StringUTF16.hashCode`/`getChar` |
| a partial class load (RKC16N.7's shape) | `StringUtf16ClassShapeProbe` | **no** — 85 declared methods, `HI_BYTE_SHIFT=0`, `LO_BYTE_SHIFT=8`, identical to HotSpot |
| `getChar` itself | `StringUtf16DispatchProbe` invokes it reflectively | **no** — returns `03A3 039F 03A3`, and `length` returns 3 |

That last row is what turned it: `getChar` and `length` were both correct when
called directly while `hashCode` was wrong, so the fault was in the caller, not
the callee — and `StringUTF16.hashCode`'s only statement is the delegation.

Worth keeping: the four wrong hashes were **solved for their input sequence**
before any of this, by folding candidate readings in Python and comparing. That
named `(char) value[i]`, sign extension included, from nothing but the outputs —
and every instrument after that was confirming a specific answer rather than
searching.

## The workaround it justified is withdrawn

`String.hashCode()` was promoted to `NativeKind::Intrinsic` on 2026-08-04 *for
correctness*, because the bytecode under it was this defect. With the bytecode
correct, the registration had to argue on performance again — the argument it
was originally written for — and it loses.

A-B-B-A interleaved, three rounds, `probes/StringHashCostProbe`, identical
digests (medians, ms):

| arm | cold latin1 | cold utf16 | warm | map utf16 |
|---|---:|---:|---:|---:|
| native | 89 | 135 | 5 | 110 |
| bytecode | 95 | 149 | **2** | 111 |

Slightly faster on the first hash of a distinct string, **2-4x slower on the
cached read**, and a wash on the realistic `HashMap<String,_>` workload. Both
sides cache in the same `String.hash` field, so the warm gap is
`safe_native_call` overhead on what should be one field read. Contract §1.4's
default is the bytecode, and a review that comes back "wash" does not license
shadowing it.

**The serial measurement said the opposite about the cold columns** — native
slower on both — and interleaving reversed it. Host load drifted between the
serial blocks. That is the third time in this feature a "this native is a
performance win" claim has failed to survive measurement, and the second time
the measurement *method* changed the answer.

## Residual — CLOSED 2026-08-05 on the Linux host

The residual read: *"Re-examine with suite numbers when the Linux host is
available: this is one microbenchmark, and `String.hashCode` is hot in every
real workload. If the native comes back, it comes back with those numbers and a
`register_with_kind` stating the kind."*

Done, on the Azure Linux host, against **`dev` tip**, on two binaries built
there and differing only in whether that one registration is
`register_with_kind(.., Intrinsic)` (native survives the real-JDK drop) or
`register` (dropped, bytecode runs). **The native does not come back.**

### The application workload the residual asked for

In-VM `javac` (`com.sun.tools.javac.Main` under `--real-jdk`) compiling 60
generated classes whose constants are UTF-16 `HashMap` keys — 2,400 non-Latin-1
`String` literals plus explicit `hashCode()` calls, i.e. a corpus deliberately
weighted toward the thing being measured. A-B-B-A, two rounds, all runs 60/60
classes:

```
native    97408  95020  95599  87269 ms
bytecode  98888  86560  90650  87245 ms
```

Fully overlapping. **No app-level signal**, on a workload chosen to maximise
one.

That is not what a first, non-A-B-B-A pass said. Run A,B,A,B,A,B on a quieter
host earlier the same day it gave native 49.8 s against bytecode 71.0 s — a
clean 1.43x with every native round below every bytecode round, which is exactly
what a real effect looks like. It did not survive A-B-B-A on a loaded host.
**Three separate perf claims in this feature have now failed to survive
re-measurement, and in two of them the ordering of the runs changed the
answer.** On this host, treat any A/B that is not A-B-B-A interleaved as
unmeasured.

### The microbenchmark, independently reproduced

`probes/StringHashCostProbe`, A-B-B-A, three rounds, on the same two dev-tip
binaries (medians, ms — a second measurement by a different session, not the
one the table above this section came from):

| arm | cold latin1 | cold utf16 | warm latin1 | warm utf16 | map utf16 |
|---|---:|---:|---:|---:|---:|
| native | 100 | 148 | 6 | 7 | 120 |
| bytecode | 108 | 150 | **3** | **2** | 121 |

Same verdict as the original table and close to its numbers: a few percent on
the cold fold, **2-3.5x slower on the cached read**, a wash on the map. HotSpot
25 for scale: 15 / 16 / 0 / 0 / 15.

### The fix itself, independently verified

`probes/StringUtf16HashProbe` on a `dev`-tip binary built on the Linux host:
`agree=true` on all eight rows plus the cached second call, `GREEK` 924359,
`TURKISH` -1606790304, `SUPPLEMENT` 274045986, `LONE-SURR` 1829648, and
`SAME-CONTENT` identical across all three constructions — matching HotSpot 25
run on the same probe.

So `String.hashCode()` stays with the bytecode, and
`string_hash_code_is_left_to_the_bytecode` in
`vm/tests/wp8_10_9_string_contains_native.rs` keeps that a decision rather than
a reflex.
