# `ByteBuffer` natives: three JDK-contract divergences

**Status:** ✅ **FIXED 2026-08-05.** Retired from `docs/known-issues/`. All
three are closed; see *How they were closed* at the bottom, which also answers
the "why these were not fixed in the same change that found them" section below
— the change that did fix them is not the one that found them.

**Found** 2026-07-31 by `probes/ByteBufferBulkProbe` while verifying the
bulk-copy rewrite in tomcat/32.3.

All three **pre-date** that rewrite — confirmed by running the probe on the
pre-fix binary, which produces a byte-identical checksum to the post-fix one
(`2662755913314845620` both sides). They are recorded here because they are in
the same native family and because two of them are *silent*: the wrong answer
is a plausible value, not an error.

## 1. Absolute `get(int)` / `put(int, byte)` are not bounds-checked against the limit

`java.nio.Buffer`'s absolute accessors check the index against the **limit**,
not the capacity. CratonVM checks neither for the past-limit case and returns a
benign zero (or performs the write):

```java
ByteBuffer b = ByteBuffer.allocate(8);
b.put(new byte[] { 1, 2, 3, 4 });
b.flip();               // limit=4, capacity=8
b.get(6);               // HotSpot: IndexOutOfBoundsException.  CratonVM: 0
b.put(6, (byte) 9);     // HotSpot: IndexOutOfBoundsException.  CratonVM: writes
```

This is **deliberate** in `s2_bb_get_byte` — its comment says a negative or
out-of-range index "reads back 0 … our synthetic path stays panic-free and
returns a benign zero byte instead". That reasoning is sound for a half-built
synthetic buffer and wrong for a real `HeapByteBuffer`, and the code cannot
currently tell them apart at that point.

**Why it matters beyond spec-lawyering:** a reader that walks past the limit
gets zeros instead of an exception, so a truncated message decodes as
zero-padded rather than failing. That is the same failure shape as the Lucene
footer/checksum bug the direct-buffer fixes in this file were written for
(ES-FAIL-FAMILY-20260709) — silently plausible data.

## 2. `put(ByteBuffer src)` does not reject `src == this`

```java
ByteBuffer b = ByteBuffer.wrap(new byte[16]);
b.put(b);   // HotSpot: IllegalArgumentException.  CratonVM: copies, pos=16
```

The JDK specifies `IllegalArgumentException` when the source is the buffer
itself. CratonVM performs the copy. Since the 2026-07-31 rewrite the copy goes
through an owned intermediate buffer, so it is at least well defined rather
than an overlapping element-wise walk — but it should throw.

This one is a two-line fix and is unambiguous: any code doing it is already
broken on HotSpot.

## 3. `IndexOutOfBoundsException` subtype (benign — recorded so it is not re-investigated)

The bulk forms throw `ArrayIndexOutOfBoundsException` where the JDK throws
plain `IndexOutOfBoundsException`, for negative `off`, negative `len` and
`off + len > array.length`. AIOOBE **is** a subclass of IOOBE, so every
`catch (IndexOutOfBoundsException)` caller still matches, and the natives'
comments already call this out as intentional. **Not a defect** — listed only
because a strict type-equality differential flags it and the next person should
not spend time on it.

## Why these were not fixed in the same change that found them

The rewrite that found them was a pure performance change with a
byte-identical differential, which is what made it safe to land. Fixing 1 in
particular is *not* that: flipping absolute-accessor bounds from "benign zero"
to "throws" changes behaviour for every synthetic buffer in the VM, and the
leniency is load-bearing by design in at least one place. It needs its own
change with a full suite run behind it, not a ride-along.

## Reproduction

```
cratonvm.exe -Xmx2g -cp <probes-out> ByteBufferBulkProbe
cratonvm.exe -Xmx2g -Dprobe.verbose=1 -cp <probes-out> ByteBufferBulkProbe
```

Run the same under HotSpot and compare. The probe prints one FNV-1a checksum
over every byte produced and every exception type thrown; `-Dprobe.verbose=1`
prints the running hash per case so a divergence can be bisected to one line.

**Trap the probe itself hit:** an early version read each buffer to `capacity`
via absolute `get(int)`, which trips divergence 1 and reported a phantom
`put([BII)` difference at sizes ≥ 63. `mixBuf` now reads through a cleared
duplicate. If you extend this probe, do not use absolute accessors past the
limit unless that is what you are testing.

---

## How they were closed (2026-08-05)

Not by a change aimed at them. The `Preconditions` exception-formatter fix
([record](preconditions-ignores-the-exception-formatter-FIXED-20260805.md))
needed "a non-`String` case (an NIO buffer slice)" as its acceptance test, and
building `probes/PreconditionsFormatterProbe` walked straight into this family.

**1 — absolute accessors not bounds-checked.** Every absolute accessor on
`java/nio/ByteBuffer` in `native-builtins/src/servlet.rs` now calls one
`s2_bb_check_abs(ctx, buf, index, width)` helper implementing
`Buffer.checkIndex(i, nb)`: in range iff `0 <= index` and
`index + width <= limit`, checked arithmetic. Twelve accessors
(`get`/`put`/`getShort`/`putShort`/`getChar`/`putChar`/`getInt`/`putInt`/
`getLong`/`putLong`/`getFloat`) were affected; the probe showed
`ByteBuffer.allocate(8).get(-1)` returning `0` and `put(8, b)` being dropped on
the floor, so this was a silent out-of-range **write** as well as a read.

The concern recorded above — "the leniency is load-bearing by design in at
least one place" — turned out to be about the wrong layer. It lives in
`s2_bb_get_byte`, a byte-level primitive called from ~15 sites with no
`MethodCallResult` to raise through, and it stays exactly as it was. The check
went on the twelve *public* natives, which do have somewhere to throw. Nothing
about half-built synthetic buffers changed.

**2 — `put(ByteBuffer src)` accepting `src == this`.** Both implementations
(`servlet.rs` and `native-io/src/lib.rs`) now raise
`IllegalArgumentException("The source buffer is this buffer")`, which is
HotSpot's message verbatim.

**3 — bulk forms throwing `ArrayIndexOutOfBoundsException`.** Recorded above as
"**Not a defect** — AIOOBE *is* a subclass of IOOBE, so every
`catch (IndexOutOfBoundsException)` caller still matches."

**That reading was too generous, and it is worth keeping as the mistake it
is.** A subclass satisfies the *widest* catch and breaks every narrower one,
plus `instanceof` and `getClass()`. The same argument appeared verbatim at
three independent sites in this codebase, and one of them
— `Preconditions`' null-formatter fallback — was actively breaking
`catch (StringIndexOutOfBoundsException)` in application code. The real reason
these sites reached for AIOOBE was that `RuntimeError` had **no
`IndexOutOfBoundsException` variant at all**; the absence had been routed
around 21 times as `IllegalArgumentException { message: "IndexOutOfBoundsException" }`,
which is not even in the right hierarchy. The variant now exists and all of
them name the real class.

Two more of the same shape were found alongside and fixed:
`IllegalStateException` carrying the *strings* `"BufferUnderflowException"` and
`"BufferOverflowException"` at eight sites, where `RuntimeError` had had both
variants the whole time.

### Verification

`probes/ByteBufferBulkProbe` on CratonVM now produces
**`7040159201000546794`** — byte-identical to HotSpot 25, where before it
produced `2662755913314845620`. That checksum covers every byte produced *and
every exception type thrown*, so it is the direct measurement this record asked
for.

`probes/PreconditionsFormatterProbe` additionally pins the class and message of
all three divergences (its `NIO contract neighbours` block), against HotSpot.
