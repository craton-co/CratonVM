# `java.lang.foreign`: the last three methods that answered `AbstractMethodError`

**Status:** FIXED (2026-08-13). The audited FFM surface now answers 46 of 46,
the same as HotSpot JDK 25. Filed 2026-08-13 from the audit the retired
`memorysegment-asbytebuffer-unimplemented-20260812` write-up asked for.

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

## What the audit found, and how it closed

46 calls across `MemorySegment`, `Arena`, `MemoryLayout` and `ValueLayout`.
HotSpot answers all 46. CratonVM answered **31**, with 15 `AbstractMethodError`s.

| | ok | AbstractMethodError | other |
|---|---|---|---|
| at the audit | 31 | 15 | 0 |
| after the first sweep | 43 | 2 | 1 `ClassCastException` |
| after a sibling session's six | 43 | 2 | 1 |
| **now** | **46** | **0** | **0** |

Closed in the first sweep: `asByteBuffer`, `asReadOnly`, `asSlice(long)`,
`copyFrom`, `mismatch`, `getString`×2, `setString`×2, `toArray`×8,
`asOverlappingSlice`, and `byteSize()`/`byteAlignment()` on the `MemoryLayout`
interface — the last of which also unblocked `sequenceLayout`, `paddingLayout`
and `unionLayout`.

Closed by a sibling session (`0175ee8a2`), from a second 42-call walk:
`maxByteAlignment`, `heapBase`, `isAccessibleBy`, `asSlice(JJJ)`,
`asSlice(J,MemoryLayout)`, `isLoaded`/`load`/`unload`/`force`, plus
`Arena.allocateFrom(String)` — which had been residual 3's second half here,
reporting `byteSize() == 0` and never writing the bytes.

Closed here — the three this page was filed for:

### 1 & 2. `elements(MemoryLayout)` and `spliterator(MemoryLayout)`

Implemented **lazily**, which is what this page argued for: the slice for
element *i* is minted when *i* is reached, so a segment with more elements than
fit in memory as separate `MemorySegment` objects still streams. A materialised
list of slices would have answered the same two calls and been wrong for
exactly the case these methods exist for.

Two decisions carry it:

* **The splitter has its own receiver class**,
  `java/lang/foreign/MemorySegment$SegmentSplitter`, not `java/util/Spliterator`.
  That name is already the runtime class of the array-backed *collections*
  spliterators, whose natives `native-collections` registers **after** the FFM
  registrar runs — so re-registering `tryAdvance` there would have taken over
  every `ArrayList` spliterator in the VM. A distinct class keeps the two apart
  with no ordering dependency and no shape-sniffing.
* **`elements` reuses the existing lazy-stream carrier.** `NativeContext` has no
  `invoke_static`, so `StreamSupport.stream(spliterator, false)` cannot be
  called from a native — but its CratonVM override
  (`service_loader::native_stream_support_stream_from_spliterator`) already
  builds, for any non-synthetic spliterator, a `java/util/stream/Stream` carrier
  with a null element array in slot 0 and the spliterator parked in the lazy
  slot 2. `native-collections` drains that on demand. Building the same shape
  directly is what makes `elements().forEach(...)` interleave `tryAdvance` and
  `accept` instead of buffering.

Measured against HotSpot with a 16-row probe — element order and values,
`byteSize` per slice, characteristics (17744 =
`NONNULL|SUBSIZED|SIZED|IMMUTABLE|ORDERED`), `estimateSize`,
`getExactSizeIfKnown`, `hasCharacteristics`, partial drain, drain past the end,
`trySplit` halves and their contents, `trySplit` after advancing, and
`forEachRemaining` after a partial drain. All 16 agree.

Two JDK behaviours worth naming because they are not what you would write:

* `estimateSize()` returns `elemCount`, **not** `elemCount - currentIndex` — a
  half-drained splitter still reports its original size. Measured: `tryAdvance`
  ×3 over 8 elements still answers 8 on HotSpot.
* `trySplit` gives the LOW half away and keeps the odd element, so 5 elements
  split 2/3, not 3/2. Pinned by `pe_split_bounds`' unit tests, which assert the
  two halves tile the segment for every count 1..64 × every element size.

### 3. `allocateFrom(ValueLayout$OfX, X... elements)`

The seven array overloads are `default` methods on `SegmentAllocator`, so real
JDK bytecode ran for them, and it ends in
`((AbstractMemorySegmentImpl) segment).copyFrom(...)` — a cast that can never
succeed while segments are interface-shaped. That is why this one presented as a
`ClassCastException` from inside the JDK rather than as the `AbstractMethodError`
its neighbours did, and why the interface audit did not catch it by shape. A
native per descriptor keeps that bytecode from running at all.

Round-tripped against HotSpot for all seven element types plus the empty array:
byte/short/char/int/float/long/double, including `Long.MIN_VALUE`, negative
floats and out-of-`byte`-range shorts. Byte-identical output.

Also fixed on the way: `MemoryLayout.paddingLayout(0)` was accepted where the
JDK refuses it at the factory (`IllegalArgumentException: Invalid byte size: 0`),
so a zero-size layout leaked to consumers that each had to re-check it.

## Known cosmetic divergence

`Spliterator.getComparator()` on an unsorted source throws the right type
(`IllegalStateException`) with an **empty** message where HotSpot's is `null`:
`RuntimeError::IllegalStateException` carries a `String`, not an
`Option<String>`, so a null message is not expressible without widening that
enum. Type, timing and call site all match.

## Repro

```bash
# FfmAudit.java walks the surface and prints OK / THROW / NO-CODE per call;
# SplProbe.java and AllocFromProbe.java diff the two new surfaces value by value.
javac -d . FfmAudit.java SplProbe.java AllocFromProbe.java
java --enable-native-access=ALL-UNNAMED -cp . FfmAudit           # 46 ok
<cv-bin> --java-home <jdk25> --enable-native-access=ALL-UNNAMED -cp . FfmAudit
```
