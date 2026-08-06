# `java.nio.Buffer`'s constructor does not validate `position`/`limit`, though the setters do

**Status:** OPEN. Low severity — no data is fabricated; the affected call
returns an empty buffer where HotSpot throws.

**Reproducer:** `probes/CharBufferWrapProbe`, the
`wrap(str).subSequence(3,2)` row.

```
                                        HotSpot 25                        CratonVM
CharBuffer.wrap(str).subSequence(3,2)   IndexOutOfBoundsException | null  pos=3 lim=2 rem=0
```

## Why it happens

`StringCharBuffer.subSequence` does **not** range-check `start > end` itself.
It builds the result and lets the constructor complain:

```java
public final CharBuffer subSequence(int start, int end) {
    try {
        int pos = position();
        return new StringCharBuffer(str, -1, pos + checkIndex(start, pos),
                                    pos + checkIndex(end, pos), capacity(), offset);
    } catch (IllegalArgumentException x) {
        throw new IndexOutOfBoundsException();
    }
}
```

`checkIndex(start, pos)` passes for both 3 and 2, so the throw has to come from
`Buffer`'s constructor:

```java
Buffer(int mark, int pos, int lim, int cap, MemorySegment segment) {
    if (cap < 0) throw createCapacityException(cap);
    this.capacity = cap;
    this.segment = segment;
    limit(lim);        // <- IllegalArgumentException if lim > cap
    position(pos);     // <- IllegalArgumentException if pos > limit
    ...
}
```

**The validation itself is present and correct in CratonVM.** Called directly,
every buffer type throws HotSpot's exception with HotSpot's exact message:

```
IntBuffer.allocate(5).position(9)           IllegalArgumentException: newPosition > limit: (9 > 5)
LongBuffer.allocate(5).position(9)          IllegalArgumentException: newPosition > limit: (9 > 5)
DoubleBuffer.allocate(5).position(9)        IllegalArgumentException: newPosition > limit: (9 > 5)
ShortBuffer.allocate(5).limit(9)            IllegalArgumentException: newLimit > capacity: (9 > 5)
CharBuffer.allocate(5).limit(3).position(4) IllegalArgumentException: newPosition > limit: (4 > 3)
```

It does not fire when `Buffer.<init>` makes the same call on `this`. That is
the whole defect, and it is why this is filed as a **constructor-dispatch**
problem rather than a `java.nio` one: the interesting question is what
`limit(lim)` / `position(pos)` resolve to when the receiver is a
partially-constructed object whose class overrides them covariantly.

Two facts worth starting from:

* `java/nio/CharBuffer.position(I)Ljava/nio/CharBuffer;` is a registered
  native, and `--dump-native-registry` reports `invocations=2` over a probe run
  that constructs many buffers — exactly the two *explicit* `position(int)`
  calls the probe makes. So the constructor's call is not reaching the
  covariant override (nor, therefore, the compiler-generated bridge that would
  forward to it).
* `java/nio/Buffer.position(I)Ljava/nio/Buffer;` has no native and its real
  bytecode does validate, as the table above shows. So whichever of the two the
  constructor reaches, it should have thrown.

## What must change

Find out what `invokevirtual limit(int)` / `position(int)` inside
`Buffer.<init>` actually dispatches to in this VM, and why the same bytecode
validates from a user call site and not from there. Candidates, cheapest first:

1. covariant-bridge dispatch (`CharBuffer.position(int)` returns `CharBuffer`,
   so javac emits a `Buffer position(int)` bridge — does CratonVM build and
   dispatch through it?);
2. a `<init>`-specific dispatch path that resolves against the *declaring*
   class rather than the receiver's;
3. field-read ordering — `this.limit` being read before the `limit(lim)` call's
   write is visible.

## Verification when fixed

`probes/CharBufferWrapProbe`'s `wrap(str).subSequence(3,2)` row, which wants
`java.lang.IndexOutOfBoundsException` with a **null** message, plus
`CharBuffer.wrap(str, 0, 5).subSequence(3, 2)`, which takes the identical path
through a `wrap` this VM has never intercepted — so it is the cleaner control.

Do not fix this by adding a range check to `StringCharBuffer.subSequence`: the
JDK deliberately does not have one there, and a check in the wrong place would
leave the constructor still not validating for every other caller.

## History

Found 2026-08-06 while closing
[`charbuffer-wrap-string-subsequence-does-not-bounds-check`](../internal/charbuffer-wrap-string-subsequence-does-not-bounds-check-FIXED-20260806.md),
which took that probe from 44 divergences to this one. Confirmed pre-existing:
`wrap(str, 0, 5).subSequence(3, 2)` did not throw on the pre-change binary
either, and that path involved no CratonVM native at any point.
