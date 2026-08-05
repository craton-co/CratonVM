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

## Residual

Re-examine with suite numbers when the Linux host is available: this is one
microbenchmark, and `String.hashCode` is hot in every real workload. If the
native comes back, it comes back with those numbers and a `register_with_kind`
stating the kind — `string_hash_code_is_left_to_the_bytecode` in
`vm/tests/wp8_10_9_string_contains_native.rs` makes that a decision rather than
a reflex.
