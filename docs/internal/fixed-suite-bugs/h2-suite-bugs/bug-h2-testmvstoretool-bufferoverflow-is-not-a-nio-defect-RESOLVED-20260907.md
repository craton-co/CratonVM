# `TestMVStoreTool`'s `BufferOverflowException` is not an `nio`/`String` defect — 2026-09-07

**RETIRES `docs/known-issues/h2/testmvstoretool-bufferoverflow-writing-a-chunk-under-g1-20260907.md`.**

That page said the suspects were *"whatever CratonVM computes differently about
a `String`'s length or a `ByteBuffer`'s remaining capacity on this path, NOT the
GC"*, and left open *"whether it is a face of heap corruption or an independent
`nio`/`String` defect"*. **The first half is refuted and the second half is
answered: the arithmetic on this path cannot overflow, and both halves of the
contract it rests on are proven sound by probes that now live in the tree.**
What is left is the page's own alternative — a reference naming memory that is
not the object the check measured — which is
`docs/known-issues/tomcat/g1-eight-byte-write-at-a-live-objects-base-20260906.md`'s
family, and whose scope line already claims this workload.

| | |
|---|---|
| **Status** | **RESOLVED as a re-attribution.** No `nio`/`String` defect exists on this path. Not reproduced on Linux; the residual belongs to the stale-reference family, not here. |
| **Where** | Azure host 2 (`20.80.105.49`), Linux, dev tip `4d7108203`, H2 2.4.249 at `apps/h2database/h2`. |
| **Probes** | `probes/MvsWriteBuffer.java`, `probes/MvsGrowBarrier.java`, `probes/MvsCreate.java`. |

---

## 1. Why the arithmetic cannot overflow

H2 has exactly six call sites that write a string's characters into a chunk,
and **all six go through one method**:

```
$ grep -rn "writeStringData\|putStringData" --include=*.java src/
main/org/h2/mvstore/db/ValueDataType.java:411      buff.put(...).putStringData(s, len);
main/org/h2/mvstore/db/ValueDataType.java:567      buff.putVarInt(len).putStringData(s, len);
main/org/h2/mvstore/type/StringDataType.java:69    buff.putVarInt(len).putStringData(s, len);
main/org/h2/mvstore/type/MetaType.java:59          .putStringData(className, len);
main/org/h2/mvstore/type/ObjectDataType.java:1160  buff.putStringData(s, len);
main/org/h2/mvstore/WriteBuffer.java:74            public WriteBuffer putStringData(String s, int len)
```

and that method is two lines:

```java
public WriteBuffer putStringData(String s, int len) {
    ByteBuffer b = ensureCapacity(3 * len);
    DataUtils.writeStringData(b, s, len);
    return this;
}
```

`writeStringData` writes **at most three bytes per character** and loops
`for (int i = 0; i < len; i++)` over the **same `len`** the buffer was sized
from. So the buffer's capacity is derived from the number that bounds the loop:
no disagreement about a `String`'s length is expressible here, because there is
only one length and both sides use it. `s.length()` could return anything at all
and the two would still agree.

**The one and only way to reach `BufferOverflowException` from this line is for
`ensureCapacity(3 * len)` to hand back a buffer with fewer than `3 * len` bytes
remaining.** That is the whole hypothesis space, and it has two halves.

## 2. Half one: does `ByteBuffer` report `remaining` correctly?

`probes/MvsWriteBuffer.java` drives H2's **real** `org.h2.mvstore.WriteBuffer`
through exactly `ObjectDataType.StringType.write`'s sequence — the tag byte, the
var-int length, then `putStringData` — for 2 000 000 rounds, plus 20 000 rounds
of the 3 000-character string `TestMVStoreTool` itself builds
(`BIG_STRING_WITH_C`), with a `clear()` cadence that forces the buffer to be
re-grown rather than settling.

Clean on **G1, ZGC and Generational, with the JIT and with `--nojit`**.

A separate arm checked the `ByteBuffer` primitives `WriteBuffer.grow` composes —
`allocate`, `flip`, the bulk `put(ByteBuffer)`, `remaining`, `position`, `limit`,
`capacity` — against the JDK contract directly (does the bulk put advance the
destination position by exactly the source's remaining? do the copied bytes
land?). All agree.

## 3. Half two: does `ensureCapacity` return the buffer `grow` installed?

This is the interesting half, because `ensureCapacity` is a **field re-read
across a call that reassigns the field**:

```java
private ByteBuffer ensureCapacity(int len) {
    if (buff.remaining() < len) { grow(len); }   // grow() assigns this.buff
    return buff;                                  // must be the NEW one
}
```

A compiler that kept `this.buff` in a register across `grow(len)` would return
the OLD, too-small buffer, and the very next `put` would raise
`BufferOverflowException` at exactly the reported frame. That is a clean,
testable mechanism and it was the leading candidate.

`probes/MvsGrowBarrier.java` re-implements that shape with no H2 on the
classpath, asserts on **every** round that the returned buffer really has
`3 * len` remaining, and resets the field every 97 rounds so `grow()` runs
**4 124 times inside a JIT-hot loop** instead of settling after the first few.

Clean on G1, ZGC and Generational, JIT and `--nojit`, at `-Xmx256m` and
`-Xmx64m`. **The field reload is correct.**

## 4. What that leaves, and where it belongs

Both halves hold, so the buffer `ensureCapacity` measured and the buffer
`writeStringData` wrote into cannot be the same live object with disagreeing
metadata. They can still be *different memory*: a `WriteBuffer.buff` slot naming
a block that was freed, relocated or reused is exactly the shape
`g1-eight-byte-write-at-a-live-objects-base-20260906.md` documents — that page's
own table lists four Java-visible faces of one defect (`OutOfMemoryError`, two
`ClassCastException` shapes, `EXCEPTION_ACCESS_VIOLATION`) and its scope line
already names this workload's `BufferOverflowException` alongside them.

**This page is therefore folded into that family rather than carried as an
independent `nio` defect.** The retired page's instruction — *"Do not quote this
page as a GC defect until that run exists"* — is discharged in the other
direction: it is not an `nio` defect either, and the only remaining producer is
the one that page is about.

## 5. Not reproduced on Linux, and what the attempt was worth

| arm | runs | `BufferOverflowException` |
|---|---:|---|
| `org.h2.test.store.TestMVStoreTool`, G1, `-Xmx256m` | 1 | none — `rc=124`, TIMEOUT at 2 401 s, **still in the create phase** after 8 192 GC pauses |
| `MvsCreate` 25 000–200 000, G1 / ZGC / Generational, `-Xmx64m`…`-Xmx1g`, JIT and `--nojit` | 44 | none |
| `MvsCreate` 500 000, `-Xmx256m`: G1 ×3, Generational ×3, G1 + `CRATONVM_G1_JIT_MARK_DRIVER=1` ×2 (all `rc=0`), ZGC ×3 (OOM, §below) | 11 | none |
| `MvsWriteBuffer` / `MvsGrowBarrier` | 8 | none |

The ZGC arms of the third row are not trials for this: they die in 9-10 s to a
separate, now-characterised defect
(`docs/known-issues/h2/zgc-oom-on-mvstore-is-the-unregistered-entry-frame-blocking-compaction-20260907.md`),
long before the chunk-write path has been exercised enough to mean anything.
The G1 and Generational arms are the ones that ran the write path to completion,
eight times, at half a million entries each.

**The full-class arm is not a trial and should not be counted as one.** It never
leaves the create phase, so it never reaches the `MVStoreTool.dump` /
`MVStore.compact` calls the original stack sits under; a TIMEOUT there is
evidence about throughput
(`docs/internal/performance/h2-mvstoretool-create-phase-is-mutator-side-address-validation-20260907.md`),
not about this. `MvsCreate` is what gives the write path repetitions: every
`commit()` serialises chunks through
`FileStore.serializeToBuffer` → `Page.writeUnsavedRecursive` →
`ObjectDataType.write` → `writeStringData`, which is the reported stack from the
bottom up.

Absence over 64 runs is not proof — three passes were never proof of anything in
this codebase and sixty-four are not either. It is stated as what it is, next to
a mechanism that IS proven: the arithmetic cannot produce this exception, so the
next sighting is a report about a reference, and it should be filed against the
stale-reference page.
