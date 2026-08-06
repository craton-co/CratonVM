# `ByteBuffer` natives: three JDK-contract divergences

**Status:** ✅ **FIXED** 2026-08-05 on `claude/bytebuffer-jdk-contract-e3e6c6`.
Found 2026-07-31 by `probes/ByteBufferBulkProbe` while verifying the bulk-copy
rewrite in tomcat/32.3.

`ByteBufferBulkProbe` is now **byte-identical to HotSpot**: checksum
`-4407322716854397411` on both, across all 64 cases, with `-Dprobe.verbose=1`
agreeing line for line.

| | before | after |
| --- | --- | --- |
| checksum (probe as it stood 2026-07-31) | `2662755913314845620` vs HotSpot `7040159201000546794` | — |
| checksum (probe extended with the divergence-1 cases) | — | `-4407322716854397411` on both |

All three divergences **pre-dated** the bulk-copy rewrite, confirmed at the
time by a byte-identical checksum on the pre-fix binary.

Sibling page: the direct-buffer reclamation bug found alongside this one was
fixed independently by a concurrent session — see
`direct-bytebuffers-are-never-reclaimed-FIXED-20260805.md` and its still-open
residual `known-issues/direct-memory-still-exhausts-under-sustained-churn-20260805.md`.

## 1. Absolute `get(int)` / `put(int, byte)` were not bounds-checked against the limit — FIXED

`java.nio.Buffer`'s absolute accessors check the index against the **limit**,
not the capacity. CratonVM checked neither for the past-limit case and returned
a benign zero (or performed the write):

```java
ByteBuffer b = ByteBuffer.allocate(8);
b.put(new byte[] { 1, 2, 3, 4 });
b.flip();               // limit=4, capacity=8
b.get(6);               // was: 0.  now: IndexOutOfBoundsException, as HotSpot
b.put(6, (byte) 9);     // was: wrote.  now: IndexOutOfBoundsException
```

**Why it mattered beyond spec-lawyering:** a reader that walked past the limit
got zeros instead of an exception, so a truncated message decoded as
zero-padded rather than failing — the same silently-plausible failure shape as
the Lucene footer/checksum bug the direct-buffer fixes were written for
(ES-FAIL-FAMILY-20260709).

**The fix, and the objection the original record raised.** That record said the
leniency was "deliberate in `s2_bb_get_byte`", that the reasoning "is sound for
a half-built synthetic buffer and wrong for a real `HeapByteBuffer`", and that
"the code cannot currently tell them apart at that point". All true — *at that
point*. `s2_bb_get_byte`/`s2_bb_put_byte` are private byte-level primitives
called from ~15 sites (`s2_bb_read2/4/8`, `s2_bb_write2/4/8`, the typed-view
accessors, the char-decoding loop), most of which have already done their own
position/limit arithmetic and some of which deliberately walk with a negative
sentinel. **They are unchanged.**

The check went in one layer up, in the *public* absolute accessors, as
`s2_bb_check_index`, gated on `s2_bb_storage(..).is_some()`. At the
registration layer the two cases the record said were indistinguishable are
distinguishable: a buffer with a resolvable heap array or direct address is a
real buffer whose `limit` means something, and a storage-less synthetic keeps
the benign-zero behaviour it has always had.

Applied to the whole absolute family, not just the two the record named, since
they share one contract: `get(I)B`, `put(IB)`, `getShort(I)`/`putShort(IS)`,
`getChar(I)`/`putChar(IC)`, `getInt(I)`/`putInt(II)`,
`getLong(I)`/`putLong(IJ)`, `getFloat(I)`. The multi-byte forms check
`index + width <= limit`, so a straddling read throws even when the index
itself is in range.

Note this only ever affected buffers whose class is literally
`java/nio/ByteBuffer` — the ones our own `allocate`/`wrap` mint. A real
`HeapByteBuffer` or `DirectByteBuffer` overrides these methods, so real JDK
bytecode (with the JDK's own `checkIndex`) already won there.

## 2. `put(ByteBuffer src)` did not reject `src == this` — FIXED

```java
ByteBuffer b = ByteBuffer.wrap(new byte[16]);
b.put(b);   // was: copied, pos=16.  now: IllegalArgumentException, as HotSpot
```

Checked first, ahead of the read-only test, matching the JDK's own order.

## 3. `IndexOutOfBoundsException` subtype — FIXED (the original record was too generous here)

The bulk forms threw `ArrayIndexOutOfBoundsException` where the JDK throws
plain `IndexOutOfBoundsException`, for negative `off`, negative `len` and
`off + len > array.length`. The original record filed this as **"Not a
defect"** on the grounds that AIOOBE is a subclass, so every
`catch (IndexOutOfBoundsException)` caller still matches.

That reasoning is half right and was the wrong call:

* it is wrong in the *subclass* direction, which is the direction that breaks a
  `catch` — `catch (ArrayIndexOutOfBoundsException)` around a buffer operation
  matched on CratonVM and missed on a real JVM; and
* leaving it meant the differential this doc exists to serve could **never**
  go green, so the checksum stayed permanently red and stopped being able to
  detect anything new.

Fixed by adding `RuntimeError::IndexOutOfBoundsException`
(`types/src/error.rs`) and using it in `get([BII)` / `put([BII)`.

### The knock-on: `Preconditions`

Extending the probe with a DIRECT receiver (`absDirectPastLimit`) surfaced one
more mismatch, in a *different* component: a direct buffer's absolute
`get(int)` correctly bails to real `DirectByteBuffer` bytecode, which lands on
`Buffer.checkIndex` → `jdk/internal/util/Preconditions.checkIndex`, whose
CratonVM override also threw AIOOBE. All five `Preconditions` overrides now
throw plain `IndexOutOfBoundsException`, which is what the real
`Preconditions.outOfBounds` does when the `oobef` formatter is absent — and it
is always absent here, because `Preconditions.<clinit>` is deliberately
suppressed.

That closes **half** of `known-issues/preconditions-ignores-the-exception-
formatter.md` (defect 2, the fallback class). Defect 1 — the formatter itself
being discarded — remains open there and is currently unobservable for the
same `<clinit>` reason. Verified no String-domain regression:
`StringUtf16HashProbe` and `StringPolicyMatrixProbe` are byte-identical
between the pre- and post-fix binaries (the `String` callers go through the F4
interception in `lang_string.rs` and never reach `Preconditions`).

## Reproduction

```
cratonvm.exe -Xmx2g -cp <probes-out> ByteBufferBulkProbe
cratonvm.exe -Xmx2g -Dprobe.verbose=1 -cp <probes-out> ByteBufferBulkProbe
```

Run the same under HotSpot and compare. The probe prints one FNV-1a checksum
over every byte produced and every exception type thrown; `-Dprobe.verbose=1`
prints the running hash per case so a divergence can be bisected to one line.

**Trap the probe itself hit:** an early version read each buffer to `capacity`
via absolute `get(int)`, which tripped divergence 1 and reported a phantom
`put([BII)` difference at sizes ≥ 63. `mixBuf` reads through a cleared
duplicate instead. That workaround is still there and must stay — `mixBuf`
measures the bulk paths, so it has to stay inside the limit. The
`absGetPastLimit` family at the bottom of `main` now tests the past-limit
behaviour deliberately.
