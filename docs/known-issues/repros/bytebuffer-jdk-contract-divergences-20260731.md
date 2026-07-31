# `ByteBuffer` natives: three JDK-contract divergences

**Status:** 🔴 **OPEN**, found 2026-07-31 by `probes/ByteBufferBulkProbe` while
verifying the bulk-copy rewrite in
[tomcat/32.3](../../internal/fixed-suite-bugs/tomcat/32-doc04-residual-perf-assertions-CLOSED.md).

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
