# `String.hashCode()` on a UTF-16 string hashes the backing BYTES, sign-extended, not the code units

**Status:** OPEN. Found 2026-08-04 while removing the forced-native
`java/lang/String` policy
([`FIXED` record](../internal/forced-native-string-policy-two-lists-that-disagree-FIXED-20260804.md)).
Pre-existing; it was **unreachable** until then because a registered native
shadowed `String.hashCode()` at every dispatch site.

**Reproducer:** `probes/StringUtf16HashProbe`.

## What is wrong

For any `String` whose backing array is UTF-16 (`coder == UTF16`, i.e. any
string containing a code unit `> 0xFF`), the real-JDK `String.hashCode()`
bytecode returns a value that is not the JLS hash.

```
GREEK       len=3 latin1able=false units=[03A3 039F 03A3]
            hashCode=62956255  jlsFromCharAt=924359  jlsFromArray=924359  agree=false
TURKISH     len=6 units=[0054 0130 0054 004C 0045 0131]
            hashCode=-1888685079  jls=-1606790304  agree=false
SUPPLEMENT  len=6 units=[0061 0062 D801 DC01 0063 0064]
            hashCode=-1514954604  jls=274045986  agree=false
ONE-UTF16   len=1 units=[0100]
            hashCode=256  jls=256  agree=TRUE
```

HotSpot 25 agrees with the JLS column on every row.

## The object is not corrupt — only the hash is

This is the part that makes the defect narrow and worth stating precisely.
`length()`, `charAt(i)` for every `i`, `toCharArray()` and `equals` on these
same objects **all agree with HotSpot**; the probe's `units=[...]` column is
read back through `charAt` and is exactly right, unpaired surrogates included.
Three strings with the same content built three different ways (`new
String(char[])`, `+` concatenation, `substring`) are `equals` to each other and
hash to the same wrong value.

So the fault is inside what `hashCode()` dispatches to, not in the string.

## What it is actually hashing

`String.hashCode()` is `isLatin1() ? StringLatin1.hashCode(value)
: StringUTF16.hashCode(value)`, and `StringUTF16.hashCode` is
`for (i in 0 until value.length >> 1) h = 31*h + getChar(value, i)`.

Solving the four observed hashes for the input sequence that produces them
gives, for all four, **the first `length()` bytes of the backing array, each
sign-extended to a `char`** — that is, `(char) value[i]`, where the correct
expression is `getChar(value, i)`:

| string | backing bytes (LE) | sequence actually hashed |
|---|---|---|
| `ΣΟΣ` | `A3 03 9F 03 A3 03` | `FFA3 0003 FF9F` |
| `x\uD801y` | `78 00 01 D8 79 00` | `0078 0000 0001` |

Two independent errors compose here, and both matter:

* **the index is not doubled** — byte `i` instead of the byte *pair* at `2i`,
  which is why the iteration count (`value.length >> 1`) is right while the
  data is not;
* **the byte is not masked** — `(char) value[i]` sign-extends, so `0xA3`
  becomes `0xFFA3`. `ONE-UTF16` passes only because its single code unit
  `0x0100` happens to survive both errors.

That reading is `getChar` degenerating to a raw signed byte load. It is neither
`StringUTF16.getChar` (two bytes, shifted by `HI_BYTE_SHIFT`/`LO_BYTE_SHIFT`)
nor `StringLatin1.getChar` (one byte, masked `& 0xff`) — note that
`StringLatin1.getChar` **is** registered `Intrinsic` and is correct, so this is
not simply the Latin-1 accessor being used on a UTF-16 array.

## Why nobody saw it

`register_essential_natives` registered a `java/lang/String.hashCode()I`
native, and — before 2026-08-04 — `resolve_step1_native` dispatched it at every
call site regardless of the forced-native lists. The bytecode was never
executed, in either mode, so a `String.hashCode` differential could not fail.

## Blast radius

Every non-Latin-1 `String` used as a hash key. `HashMap`/`HashSet`/
`ConcurrentHashMap` lookups are self-consistent (a wrong hash is still a
*stable* hash and `equals` is correct, so a map keyed and probed within one VM
still works), which is exactly why this can sit unnoticed. It breaks:

* anything that persists or transmits a hash, or compares one against a value
  computed elsewhere;
* `Objects.hash(...)` / record `hashCode` / `Arrays.hashCode` over strings, and
  any test asserting a literal expected hash;
* bucket distribution for non-ASCII keys (a correctness-adjacent perf issue).

## What must change

Fix `StringUTF16.getChar(byte[], int)` — or whatever CratonVM resolves it to —
so it reads the code unit at `2i`/`2i+1` masked and shifted by the
`HI_BYTE_SHIFT` / `LO_BYTE_SHIFT` statics, and check that
`StringUTF16.<clinit>` actually ran to set those (the `isBigEndian()Z`
registration in the census reports `has_code: false` in the image, which is
worth confirming before assuming the statics are populated).

Then re-measure `probes/StringUtf16HashProbe` — `agree=true` on every row — and
**re-open the question of the `String.hashCode()` native**, which is currently
registered `NativeKind::Intrinsic` *because of this defect* rather than for the
~1950x caching win it was originally written for. Once the bytecode is correct,
that registration is a pure performance optimisation again and has to argue for
itself on those terms.

## Not to be confused with

`probes/StringPolicyMatrixProbe`'s `hashCode(LONE)` row also diverges, for a
**different** reason: `LONE` there is built by `+` concatenation, which loses an
unpaired surrogate to U+FFFD before `hashCode` ever runs. That is
[the concatenation defect](string-concat-loses-unpaired-surrogates.md), and a
correct `hashCode` over corrupt content is still going to differ from HotSpot.
