# CratonVM pinned `sun.misc.unsafe.memory.access`, which flipped netty onto its Unsafe fast paths where HotSpot 25 disables them

**Status:** FIXED (2026-08-13). The pin is gone; CratonVM's observable property
set now matches HotSpot's, netty takes the same code path on both VMs, and the
three prerequisites in the original fix order were closed in that order. Found
while triaging [netty investigate-batch-01](../../../known-issues/netty/investigate-batch-01.md) on the
Azure Linux host, binary built from `origin/dev` `1c4ce7d3a`.

## Symptom

`io.netty.buffer.AlignedPooledByteBufAllocatorTest` ran a different *set* of
tests on CratonVM than on HotSpot JDK 25 — not a failure, a divergence:

| | HotSpot JDK 25 | CratonVM (before) | CratonVM (now) |
| --- | --- | --- | --- |
| `found` | 49 | 49 | 49 |
| `ok` | 21 | 48 | **21** |
| `aborted` (failed assumption) | 28 | 1 | **28** |

Its `newAllocator()` opens with
`assumeTrue(PooledByteBufAllocator.isDirectMemoryCacheAlignmentSupported())`,
which is false on HotSpot and was true on CratonVM.

## Root cause — one system property

`isDirectMemoryCacheAlignmentSupported()` is `PlatformDependent.hasUnsafe()`,
and netty 4.2's `PlatformDependent0.explicitNoUnsafeCause0()` disables Unsafe
by default on Java 25+ **unless** the JDK's memory-access escape hatch has been
set:

```java
String unsafeMemoryAccess = SystemPropertyUtil.get("sun.misc.unsafe.memory.access", "<unspecified>");
if (!explicitProperty && "<unspecified>".equals(unsafeMemoryAccess) && javaVersion() >= 25) {
    reason = "io.netty.noUnsafe=true by default on Java 25+";
    noUnsafe = true;
}
```

Measured, both VMs, same classpath:

| probe | HotSpot JDK 25 | CratonVM (before) | CratonVM (now) |
| --- | --- | --- | --- |
| `System.getProperty("sun.misc.unsafe.memory.access")` | `null` | **`allow`** | `null` |
| `PlatformDependent.hasUnsafe()` | `false` | **`true`** | `false` |
| `PlatformDependent.getUnsafeUnavailabilityCause()` | `UnsupportedOperationException: sun.misc.Unsafe: unavailable (io.netty.noUnsafe=true by default on Java 25+)` | `null` | *identical to HotSpot* |

Every other input agrees (`java.specification.version` 25, `io.netty.noUnsafe`
unset, `PlatformDependent0.javaVersion()` 25). The property alone decided it.

HotSpot sets `sun.misc.unsafe.memory.access` only when the user passes
`--sun-misc-unsafe-memory-access=<mode>`; the JDK 25 default leaves it unset
(the effective mode is still "allow, with a warning"). CratonVM set it
unconditionally in `vm/src/vm/vm_init.rs`.

## Why it was pinned — and what had to close first

The pin was a workaround for JEP 498: under the JDK 25 default (`warn`) the
first legacy `sun.misc.Unsafe` memory access drops into
`Unsafe.beforeMemoryAccessSlow()`, which runs a `StackWalker.walk()` and
unconditionally dereferences `frames.get(1)`. CratonVM's StackWalker was
reported to return fewer than two frames for some native/JIT-spliced call
chains, so `List.get(1)` threw — surfacing in jctools' `MpscUnboundedArrayQueue`
(netty's per-`NioEventLoop` task queue) as "failed to create a child event
loop". Pinning `allow` makes `beforeMemoryAccess()` return at its first check.

**And the pin was load-bearing for a second, larger reason.** Forcing netty
onto HotSpot 25's actual path (`-Dio.netty.noUnsafe=true`) did not merely change
which tests were skipped; it broke the allocator outright —
`io.netty.buffer.AdaptiveByteBufAllocatorTest` went from 126/127 to **17/127**,
all 109 failures one cause: netty's non-Unsafe allocator is `CleanerJava25`, an
FFM-`Arena`-backed one, and `MemorySegment.asByteBuffer()` was unimplemented.

## Fix, in the order the original write-up demanded

**1. Close the FFM gap first.** `MemorySegment.asByteBuffer()` now mints a
direct `ByteBuffer` over the segment's own memory by running the JDK's own
`DirectByteBuffer(long, int, Object, MemorySegment)` constructor — the one
`NativeMemorySegmentImpl.makeByteBuffer()` reaches through `NIO_ACCESS` — so
every `Buffer` invariant is inherited rather than restated, and the segment
passed as the constructor's `MemorySegment` argument is what keeps the arena
reachable for the buffer's lifetime. `asReadOnly()` and `asSlice(long)` came
with it (the first is what `asByteBuffer` consults for a read-only view; the
second simply had no registration). Measured, same class as above, same flag:

```
                                        ok    failed
AdaptiveByteBufAllocatorTest, -Dio.netty.noUnsafe=true
  before                                17       109
  after                                126         0     (and 1 aborted, matching the default arm)
```

Zero `AbstractMethodError`s in the whole run. A full audit of the FFM surface
came with it — 46 calls on both VMs, 15 `AbstractMethodError`s closed to 2; the
remainder are recorded in
ffm-elements-spliterator-and-allocatefrom-gaps (retired: `ffm-elements-spliterator-and-allocatefrom-gaps-20260813`).

**2. Stop setting the property.** `vm_init` no longer seeds it, and `vm-cli`
rewrites `--sun-misc-unsafe-memory-access=<mode>` — HotSpot's own launcher flag,
which CratonVM did not accept at all — into the `-D` form. The property now has
exactly one source: the user. The `sun/misc/Unsafe` post-clinit repair in
`vm_util.rs` already read the property and already fell back to `WARN`, the
JDK's own default, so nothing forces `ALLOW` internally either.

The StackWalker shortfall that motivated the pin **did not reproduce**. The
probe from the original report (`StackWalker.walk` at depths 1 and 2, then
legacy `Unsafe.getInt`/`putLong` under `warn`) answers identically on both VMs,
and no netty class regressed to "failed to create a child event loop". Rather
than keep a workaround for a symptom that no longer occurs, the pin is gone and
this note is the record of what to look for if it returns.

## What switching paths exposed — and it was not small

The original write-up's own warning ("cross-check the netty suite and Keycloak
before and after — this changes which code path a large amount of third-party
code runs") paid off immediately. netty's non-Unsafe `ByteBuf` reads and writes
go through `MethodHandles.byteArrayViewVarHandle` /
`byteBufferViewVarHandle`, and **CratonVM's implementation of both did no
bounds check whatsoever**:

| call (16-byte target, `short` view) | HotSpot JDK 25 | CratonVM (before) |
|---|---|---|
| `arrayView.get(bytes, -1)` | `ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 15` | **no throw** |
| `arrayView.set(bytes, -1, v)` | same | **no throw — an out-of-bounds write** |
| `bufferView.get(heapBuffer, 0)` | `0` | **SIGSEGV** |
| `bufferView.get(buffer, -1)` | `IndexOutOfBoundsException: Index -1 out of bounds for length 15` | silently read element 0 |
| `bufferView.set(readOnlyBuffer, 0, v)` | `ReadOnlyBufferException` | **no throw — wrote anyway** |

The heap-buffer SIGSEGV is the sharpest of them: `byte_buffer_view_addr` read
`java.nio.Buffer.address` and treated it as an absolute pointer, but on JDK 21+
that field is **16 on a heap buffer** (the array base offset, so `Unsafe` can use
one code path for both kinds) — so every heap-buffer view dereferenced address
`0x10 + index`, and the heap-array branch below it was unreachable. `hb == null`
is the field that actually distinguishes the two.

All five rows now match HotSpot byte for byte, message text included. That was
`io.netty.buffer.LittleEndianHeapByteBufTest` at 406/412 and
`BigEndianDirectByteBufTest` at 407/413 — both 412/412 and 413/413 now, matching
HotSpot. **A pristine `origin/dev` build fails those same six tests when given
`-Dio.netty.noUnsafe=true`**, so this is a pre-existing defect the pin was
hiding, not a regression from removing it.

## Result

| class | HotSpot 25 | CratonVM before | CratonVM after |
|---|---|---|---|
| `AlignedPooledByteBufAllocatorTest` | ok=21 aborted=28 | ok=48 aborted=1 | **ok=21 aborted=28** |
| `LittleEndianHeapByteBufTest` | 412/412 | 412/412 (Unsafe path) | **412/412** (same path as HotSpot) |
| `BigEndianDirectByteBufTest` | 413/413 | 413/413 (Unsafe path) | **413/413** |
| `AdaptiveByteBufAllocatorTest` `-Dio.netty.noUnsafe=true` | — | 17/127 | **126/127** |

## Suite validation

Three arms, interleaved per class: pristine `origin/dev`, this branch, and
HotSpot JDK 25 — the third being a real oracle for the first time, since both
CratonVM arms and HotSpot now run the same netty code path.

The named classes were re-run serially and ABBA-interleaved, which is the
trustworthy half of the measurement:

| class | ctl (dev) | mine | HotSpot |
|---|---|---|---|
| `AlignedPooledByteBufAllocatorTest` | ok=48 ab=1 | **ok=21 ab=28** | ok=21 ab=28 |
| `LittleEndianHeapByteBufTest` | 412/412 | **412/412** | 412/412 |
| `BigEndianDirectByteBufTest` | 413/413 | **413/413** | 413/413 |
| `Http2MultiplexTransportTest`-style `NioEventLoopTest` | ok=12 F=1 | ok=12 F=1 | ok=13 F=0 |
| `LoadClassTest` (kqueue) | 8/8 | 8/8 | 8/8 |

`NioEventLoopTest`'s one failure is unchanged by this work — the same 12/1 on
both CratonVM binaries across 16 interleaved runs.

The broad sample sweep (122 classes) is the weaker half and is reported as
such: the shared build host was at load average 130+ from other agents while it
ran, so most rows produced no `@@RESULT` on one arm or another. Of the rows
where **both** CratonVM arms produced a real result, every difference was
CratonVM moving *toward* HotSpot — `KQueueIoHandlerWakeupEventCountTest`
(mine `f3 ok0 F0` = HotSpot, ctl `f1 ok0 F1`), `HttpClientCodecTest`
(mine 15/15, ctl 14/14), `HostsFileParserTest` (mine `f3 ok3` = HotSpot, ctl
`f1 ok1`) — plus the two `ByteBuf` classes above, which the VarHandle bounds
fixes closed. No row moved away from HotSpot.

## Triage note, still true

Some netty classes gate every test on `assumeTrue(PlatformDependent.hasUnsafe())`.
Those now abort on CratonVM exactly as they do on HotSpot, which is the correct
default — and when you want to *test* the Unsafe path on both VMs, give both the
same flag:

```bash
java     --sun-misc-unsafe-memory-access=allow -cp "$CP" CratonRunner <class>
cratonvm --sun-misc-unsafe-memory-access=allow -cp "$CP" CratonRunner <class>
```

CratonVM now accepts that spelling (it previously did not, and clap rejected
it). This remains a triage instrument, not a target state.

## Repro

```bash
CP=$(sed -n 2p apps/netty-suite-runner/common.args)
cat > UnsafeWhy2.java <<'EOF'
import io.netty.util.internal.PlatformDependent;
public class UnsafeWhy2 {
    public static void main(String[] a) {
        System.out.println("prop=[" + System.getProperty("sun.misc.unsafe.memory.access") + "]");
        System.out.println("hasUnsafe=" + PlatformDependent.hasUnsafe());
    }
}
EOF
javac -cp "$CP" -d . UnsafeWhy2.java
java     -cp "$CP:." UnsafeWhy2                        # prop=[null]  hasUnsafe=false
cratonvm --java-home <jdk25> -cp "$CP:." UnsafeWhy2    # prop=[null]  hasUnsafe=false  (was [allow]/true)
```
