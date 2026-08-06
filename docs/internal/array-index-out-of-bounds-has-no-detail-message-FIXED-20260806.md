# `ArrayIndexOutOfBoundsException` from an array access carries no detail message

**Status:** FIXED 2026-08-06. Every row of the array family now matches a
HotSpot 25 control on both class and message, including the three that turned
out not to be message bugs at all.

**Reproducer:** `probes/PreconditionsFormatterProbe`, the `Array domain`
sections (2 rows before this work, 75 after).

```
                       HotSpot 25                                                CratonVM before        CratonVM now
int[] load oob         Index 9 out of bounds for length 4                        null → "Array index    Index 9 out of bounds for length 4
                                                                                 out of range: 9"
System.arraycopy oob   arraycopy: last source index 9 out of bounds for int[4]   null                   arraycopy: last source index 9 out of bounds for int[4]
Array.get(int[4],9)    ArrayIndexOutOfBoundsException, null message              NO-THROW, returned 0   ArrayIndexOutOfBoundsException, null message
```

## What it turned out to be

Three separate defects wearing one symptom, only the first of which the
original report named.

**1. The variant had no room for the message.**
`RuntimeError::ArrayIndexOutOfBoundsException` carried an index and nothing
else, and `as_java_throwable` mapped it to the no-arg constructor. It now
carries `message: Option<String>` and three constructors choose the wording,
because HotSpot has three different answers and a single blanket message cannot
be right for all of them:

| constructor | wording | who uses it |
|---|---|---|
| `aioobe(index, length)` | `Index 9 out of bounds for length 4` | every `aaload`/`aastore`/`iaload`/… in both tiers |
| `aioobe_with_message(index, msg)` | caller's, e.g. `arraycopy: …` | `System.arraycopy`, `java.util.Arrays`' range checks |
| `aioobe_index_only(index)` | `Array index out of range: 9` | a native that holds an index and no length |
| `aioobe_no_message(index)` | *(null)* | `java.lang.reflect.Array` — HotSpot's is null too |

The array-access wording is `out_of_bounds_message::check_index`, the same
function `Preconditions.checkIndex` uses — they are character-identical on
HotSpot and now cannot drift apart here.

**2. `System.arraycopy` had its own message shapes, and its own check order.**
Five out-of-bounds wordings (`source index`, `destination index`, `length … is
negative`, `last source index`, `last destination index`) plus three
`ArrayStoreException` wordings, all in `arraycopy_message`. The two `last_*`
shapes print `pos + length` **unsigned**, which is what makes an overflowing
addition render as `2147483649` rather than a negative number.

The order was also wrong, and that part was **not** a diagnosability bug:
`arraycopy(int[4], 0, long[4], 0, 9)` violates both the type and the range, and
HotSpot reports the type mismatch. This VM checked the range first and threw an
`ArrayIndexOutOfBoundsException` where a `catch (ArrayStoreException)` was
written. The probe's `arraycopy check precedence` rows pin every pairwise
ordering.

**3. `java.lang.reflect.Array` never bounds-checked at all.** This one is not a
message bug and is more serious than the one reported: the `int` index was cast
straight to `usize` and handed to a heap accessor that answers out-of-range
reads with a default and drops out-of-range writes. So

```java
Array.get(new int[4], 9)      // returned 0
Array.set(new int[4], 9, v)   // silently did nothing
Array.get(new int[4], -1)     // returned 0, via a huge usize
```

where HotSpot throws. All 18 registered `Array.*` methods funnel through ten
functions; they share one bounds-checked index helper now. The thrown exception
deliberately carries **no** message, because HotSpot's does not either — that is
measured, not an unfinished migration, and `aioobe_no_message` exists to say so
at the call site.

## Relationship to `510f12fc6`

That commit landed on `dev` while this branch was in flight and closed the same
gap partially: it kept the variant one-field and synthesised `"Array index out
of range: N"` for every AIOOBE. That is the JDK's `int`-constructor wording —
correct where only an index is known, wrong for an array access, and wrong for
`reflect.Array`, where it replaced a correct null with text. Its diagnosability
win is kept as `aioobe_index_only`, which is what the ~180 index-only native
sites now call; the two cases it could not serve get the other constructors.

`synthesised_detail_message` still exists for `NegativeArraySizeException`,
which genuinely has nothing to borrow.

## Verification

`probes/PreconditionsFormatterProbe` against `/home/victor/jdk25` (Temurin
25.0.3+9) as the oracle. Before: **56 of 118 rows differed**. After: see the
run recorded in the commit message. The probe grew the `arraycopy` variants the
original report asked for (`srcPos < 0`, `length < 0`, `last destination index`,
the per-element-type names, the overflow row) plus a JIT-warmed section, because
the JIT tier already carried the right message while the interpreter did not —
the two tiers disagreed about the same array access, and nothing was asking.

Unit tests in `types/src/error.rs` pin the three wordings against each other
(`the_three_aioobe_constructors_do_not_agree`), the unsigned `last_*` rendering,
and the `object array` element-type name.

## Still divergent, and not this bug

`CharBuffer.wrap(str).subSequence(…)` does not bounds-check — that is
`docs/known-issues/charbuffer-wrap-string-subsequence-does-not-bounds-check.md`,
a separate receiver with its own record.

`Array.getInt(Object[4], 0)` and `Array.set(int[4], 0, "x")` want
`IllegalArgumentException` with HotSpot's finer wording (`"Argument is not an
array of primitive type"`, `"argument type mismatch"`). That is the reflective
*type*-check contract, not the bounds contract, and it is untouched here.
