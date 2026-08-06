# `CharBuffer.wrap(String).subSequence(start, end)` accepts any range instead of throwing

**Status:** OPEN.

**Reproducer:** `probes/PreconditionsFormatterProbe`, the two
`CharBuffer.wrap(str).subSequence(...)` rows.

```
                                             HotSpot 25                              CratonVM
CharBuffer.wrap(str).subSequence(-1,2)  IndexOutOfBoundsException | null    NO-THROW ""
CharBuffer.wrap(str).subSequence(0,99)  IndexOutOfBoundsException | null    NO-THROW "Hello, World" + 87 NULs
```

`str` is `"Hello, World"` (length 12). The second row is the one that matters:
the call reads 87 code units past the end of the wrapped sequence and hands
them back as content.

## Why it happens

`CharBuffer.subSequence` is abstract; `StringCharBuffer` implements it as

```java
public final CharBuffer subSequence(int start, int end) {
    try {
        int pos = position();
        return new StringCharBuffer(str, -1,
                                    pos + checkIndex(start, pos),
                                    pos + checkIndex(end, pos),
                                    capacity(), offset);
    } catch (IllegalArgumentException x) {
        throw new IndexOutOfBoundsException();
    }
}
```

`Buffer.checkIndex(i, nb)` is `Preconditions.checkIndex(i, limit - nb + 1,
IOOBE_FORMATTER)`, so on a real JDK this bottoms out in `Preconditions` and —
since `native-builtins/src/preconditions.rs` — CratonVM would answer it
correctly.

It never gets there. `CharBuffer.wrap(CharSequence)` is a CratonVM native
(`native-builtins/src/phases_late/charset_buffers.rs`, and a second one in
`native-io/src/lib.rs`) that returns a **synthetic 5-field
`java/nio/CharBuffer`** — not a `StringCharBuffer`, and not carrying the field
layout the real `Buffer.limit()` reads. `subSequence` has no native, so the
real bytecode runs against a receiver whose `limit` it cannot see, the range
check passes for anything, and the copy walks off the end.

So this is a **CharBuffer layout/coverage gap**, not an exception-formatter
gap. It is filed separately from
[`preconditions-ignores-the-exception-formatter`](../internal/preconditions-ignores-the-exception-formatter-FIXED-20260805.md)
because fixing that one cannot reach it: the bytecode never asks
`Preconditions` anything.

Note the sibling rows that ARE correct, which is what localises this to
`wrap(String)` specifically rather than to CharBuffer generally:
`CharBuffer.allocate(8).slice(0, 99)` and `CharBuffer.allocate(8).charAt(9)`
both match HotSpot exactly.

## What must change

Either:

* register a `subSequence(II)` native on `java/nio/CharBuffer` that performs
  `Buffer.checkIndex(start, position())` / `checkIndex(end, position())`
  against the synthetic layout and slices it — the same shape the existing
  `ByteBufferAsCharBuffer{B,L}.subSequence` native already has; or
* make `CharBuffer.wrap(CharSequence)` produce something whose `limit` /
  `capacity` the real `Buffer` bytecode can read, so `subSequence` and every
  other unimplemented `CharBuffer` method inherit their checks for free.

The second is the larger and better fix. The first is bounded and closes the
out-of-range read.

## Verification when fixed

The two `PreconditionsFormatterProbe` rows above, class and message. Both want
`java.lang.IndexOutOfBoundsException` with a **null** message — `Buffer`'s own
formatter builds the exception with no detail string, unlike the
`Objects.check*` callers.

Watch for regressions in the callers the `wrap(CharSequence)` native was
written for: Tomcat's `MessageBytes.toBytes` (`encoder.encode(CharBuffer.wrap(charChunk))`)
and icu4j's `ICUResourceBundleReader.getStringV2`
(`bytes.asCharBuffer().subSequence(...).toString()`).
