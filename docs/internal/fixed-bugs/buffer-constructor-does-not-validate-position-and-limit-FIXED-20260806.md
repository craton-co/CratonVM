# `java.nio.Buffer`'s constructor did not validate `position`/`limit`

**Status:** FIXED 2026-08-06. Retired from `docs/known-issues/`.

**Reproducer:** `probes/BufCtor` (14 rows) and `probes/CharBufferWrapProbe`
(61 rows), both diffed against HotSpot 25.

| | before | after |
|---|---:|---:|
| `BufCtor` divergences | 9 of 14 | **0** |
| `CharBufferWrapProbe` divergences | 1 of 61 | **0** |

The `CharBufferWrapProbe` row is the one
[`charbuffer-wrap-string-subsequence-does-not-bounds-check`](charbuffer-wrap-string-subsequence-does-not-bounds-check-FIXED-20260806.md)
could not close and filed this record for.

## The record's own diagnosis was the long way round

It said:

> the interesting question is what `limit(int)` / `position(int)` resolve to
> when the receiver is a partially-constructed object whose class overrides
> them covariantly

and listed three candidate mechanisms, all about covariant-bridge dispatch. It
had good evidence for asking — `CharBuffer.position(I)Ljava/nio/CharBuffer;`
really did show `invocations=2` for two explicit calls and none from any
constructor.

The answer was one line further down the same registry dump:

```
java/nio/Buffer  <init>(IIIILjava/lang/foreign/MemorySegment;)V   inv=9
```

**`Buffer`'s constructor is itself a CratonVM native**, and it wrote the four
fields without performing any of the constructor's checks. Nothing was ever
dispatched anywhere surprising; the bytecode that would have called
`limit(int)` and `position(int)` never ran. `Buffer.limit(int)` and
`Buffer.position(int)` were correct the whole time — as the record itself
recorded, they throw HotSpot's exact exception for every buffer type when
called directly.

The tell was in the record's own data and I read past it: I filtered that dump
for `name in ('limit','position')` and never looked at `<init>`.

## What was wrong

The JDK body is

```java
Buffer(int mark, int pos, int lim, int cap, MemorySegment segment) {
    if (cap < 0) throw createCapacityException(cap);
    this.capacity = cap; this.segment = segment;
    limit(lim);       // IllegalArgumentException if lim > cap or lim < 0
    position(pos);    // IllegalArgumentException if pos > limit or pos < 0
    if (mark >= 0) { if (mark > pos) throw ...; this.mark = mark; }
}
```

and that validation is load-bearing for callers that do not check themselves.
`CharBuffer.wrap(csq, start, end)`, `CharBuffer.wrap(char[], off, len)`,
`ByteBuffer.wrap(byte[], off, len)` and `StringCharBuffer.subSequence` are all
written as

```java
try { return new ...(...); }
catch (IllegalArgumentException x) { throw new IndexOutOfBoundsException(); }
```

— **the range check *is* the constructor's**. Without it,
`CharBuffer.wrap("Hello, World").subSequence(3, 2)` returned a buffer with
position 3 and limit 2, and `CharBuffer.wrap(new char[4], 3, 2)` one running
past its own array.

## The fix

Perform the JDK's checks, in the JDK's order, with `createCapacityException` /
`createLimitException` / `createPositionException`'s exact messages. Three
sibling natives had the same shape and are fixed with it:

* `CharBuffer.allocate(-1)` and `ByteBuffer.allocate(-1)` clamped the capacity
  with `.max(0)` and returned an empty buffer, reporting success where HotSpot
  throws `capacity < 0: (-1 < 0)`;
* `ByteBuffer.wrap(array, off, len)` clamped the limit with `.min(cap)`, so
  `wrap(new byte[4], 0, 9)` silently produced a **4-byte** window instead of
  throwing.

## Turning the check on immediately found a second, older defect

`CharBuffer.allocate(12).duplicate()` began throwing

```
IllegalArgumentException: mark > position: (52784352 > 0)
```

`duplicate()` passes `markValue()` straight into the constructor, and that
number is a heap address: the buffer's `mark` was holding a **reference to its
own backing `char[]`**.

`cb_write_hb` writes the buffer state twice — once by real-JDK field name, once
by synthetic slot index. The two layouts alias. On a real-JDK
`java/nio/CharBuffer` the field order is Buffer's `mark`(0) `position`(1)
`limit`(2) `capacity`(3) `address`(4), then CharBuffer's `hb`/`offset`/
`isReadOnly` — so of the five indexed slots only 1/2/3 mean the same thing in
both. `CB_FIELD_ARRAY`(0) lands on **`mark`**; `CB_FIELD_MARK`(4) lands on
**`address`**.

`address` had been noticed (`e02262b15`, the day before) and was re-asserted
after the indexed writes. `mark` had not, and stayed corrupt — invisibly,
because nothing read it until this constructor check did.

The ByteBuffer side had already learned this: `servlet.rs::bb_write_hb` guards
its indexed writes with `s2_bb_synthetic_layout` and pins the behaviour with
`bb_write_hb_real_layout_preserves_address_and_mark`. The CharBuffer side now
does the same, with `cb_synthetic_layout` and two matching tests. **Re-asserting
the one aliased field somebody remembered is what left the other one corrupt;
not writing them at all on a real layout cannot rot that way.**

## Verification

* `probes/BufCtor` — 14 rows, byte-identical to HotSpot 25.
* `probes/CharBufferWrapProbe` — 61 rows, **0 divergences**, down from 1.
* `probes/PreconditionsFormatterProbe` — unchanged at 2 of 58, both the
  separately filed `array-index-out-of-bounds-has-no-detail-message`.
* `probes/ByteBufferBulkProbe` — checksum still `7040159201000546794`.
* `probes/StringPolicyMatrixProbe` — unchanged at 3 of 392.
* New unit tests: `cb_write_hb_real_layout_preserves_address_and_mark` and
  `cb_write_hb_pure_synthetic_layout_still_gets_indexed_fallback`.

## What this one is worth keeping

**When a dump answers your question with "not here", widen the filter before
inventing a mechanism.** The record reached for covariant-bridge dispatch,
`<init>`-specific resolution, and field-write ordering — three plausible VM
internals — when the actual answer was a row in the same table, excluded by a
`name in ('limit','position')` filter I wrote myself. A native standing in
front of a constructor is not an exotic hypothesis; it is the first thing to
rule out, and the dump rules it in or out for free.
