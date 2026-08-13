# `java.lang.foreign`: three methods still answer `AbstractMethodError`, and one wrong-cast

**Status:** OPEN (2026-08-13). Found by auditing the whole public FFM surface
one call at a time on both VMs, which is what the retired
`memorysegment-asbytebuffer-unimplemented-20260812` write-up asked for. The
audit closed 12 of 15 gaps; these are what is left.

## Why an audit was needed at all

CratonVM fabricates every `java.lang.foreign` object as an instance of the
**interface** it implements:

| | HotSpot JDK 25 | CratonVM |
|---|---|---|
| `ValueLayout.JAVA_INT.getClass()` | `…layout.ValueLayouts$OfIntImpl` | `java.lang.foreign.ValueLayout$OfInt` |
| `Arena.ofShared().allocate(16).getClass()` | `…foreign.NativeMemorySegmentImpl` | `java.lang.foreign.MemorySegment` |

So an interface method with no registered native is not a slow path or a
"missing feature" error — it is

```
java.lang.AbstractMethodError: method java/lang/foreign/MemorySegment.getString(J)Ljava/lang/String; has no Code attribute
```

thrown at the call site, naming dispatch rather than the gap. Nothing surfaces
that except calling the method, so the whole surface had to be walked.

## What the audit found

46 calls across `MemorySegment`, `Arena`, `MemoryLayout` and `ValueLayout`.
HotSpot answers all 46. CratonVM, before the sweep of fixes on the same branch:
**31 ok, 15 `AbstractMethodError`**. After: **43 ok, 2 `AbstractMethodError`,
1 `ClassCastException`.**

Fixed on the way (see the same branch): `asByteBuffer`, `asReadOnly`,
`asSlice(long)`, `copyFrom`, `mismatch`, `getString`×2, `setString`×2,
`toArray`×8, `asOverlappingSlice`, and `byteSize()`/`byteAlignment()` on the
`MemoryLayout` interface — the last of which also unblocked
`sequenceLayout(...)`, `paddingLayout(...)` and `unionLayout(...)`, whose
carriers were one slot wide and held the constructor argument where every
reader expects the size.

## What is still open

### 1. `MemorySegment.elements(MemoryLayout)` → `Stream<MemorySegment>`
### 2. `MemorySegment.spliterator(MemoryLayout)` → `Spliterator<MemorySegment>`

```
java.lang.AbstractMethodError: method java/lang/foreign/MemorySegment.elements(
  Ljava/lang/foreign/MemoryLayout;)Ljava/util/stream/Stream; has no Code attribute
```

Not implemented because a faithful one is a real `Spliterator` over the
segment: lazily split, `SIZED | SUBSIZED | IMMUTABLE | ORDERED | NONNULL`, and
`elements` is `StreamSupport.stream(spliterator(...), false)` on top of it.
A materialised `ArrayList` of slices would answer the two calls in the audit
and be wrong for the case they exist for — a segment too large to hold all its
slices at once.

### 3. `SegmentAllocator.allocateFrom(ValueLayout$OfInt, int...)`

```
java.lang.ClassCastException: class java.lang.foreign.MemorySegment cannot be
  cast to class jdk.internal.foreign.AbstractMemorySegmentImpl
```

This one gets *further* than the others: the JDK's own `allocateFrom` bytecode
runs (`MemoryLayout.byteSize()` now answers), and then casts the segment it was
handed to the internal implementation class. That cast can never succeed while
segments are interface-shaped, so the fix is a native for the seven
`allocateFrom(ValueLayout$OfX, X[])` descriptors rather than anything in the
layout code.

A second, related wrong answer with the same root: `Arena.allocateFrom(String)`
reports `byteSize() == 0` where HotSpot reports 5 for `"text"` — it allocates
but the length never reaches the carrier.

## Repro

```bash
# FfmAudit.java walks the surface and prints OK / THROW / NO-CODE per call.
javac -d . FfmAudit.java
java --enable-native-access=ALL-UNNAMED -cp . FfmAudit                     # 46 ok
<cv-bin> --java-home <jdk25> --enable-native-access=ALL-UNNAMED -cp . FfmAudit
```

## Why it matters more than it used to

Until 2026-08-13 CratonVM pinned `sun.misc.unsafe.memory.access=allow`, which
kept netty — and every other `sun.misc.Unsafe`-aware library — off the FFM
path entirely. That pin is gone (the observable property set now matches
HotSpot's), so this surface is on the default path for anything that has
dropped `sun.misc.Unsafe`, which on JDK 25 is a growing list.
