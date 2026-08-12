# `MemorySegment.asByteBuffer()` is unimplemented — netty's non-Unsafe direct allocator cannot allocate at all

**Status:** OPEN (2026-08-12). Found while triaging
[netty investigate-batch-01](investigate-batch-01.md) on the Azure Linux host
(`20.80.105.49`), binary built from `origin/dev` `1c4ce7d3a`.

## Symptom

```java
java.lang.foreign.Arena ar = java.lang.foreign.Arena.ofShared();
java.lang.foreign.MemorySegment seg = ar.allocate(64, 8);   // ok on both VMs
seg.asByteBuffer();                                          // <-- here
```

```
HotSpot JDK 25 : cap=64 direct=true
CratonVM       : java.lang.AbstractMethodError:
                 method java/lang/foreign/MemorySegment.asByteBuffer()Ljava/nio/ByteBuffer;
                 has no Code attribute
```

`Arena.allocate` works and reports the right `byteSize()`; only the
`ByteBuffer` view is missing. CratonVM's FFM support fabricates the segment as
an instance of the **interface** `java/lang/foreign/MemorySegment` (see
`native-builtins/src/phases_late/foreign_ffm.rs` and `panama.rs`), so any
interface method without a registered native surfaces as
"no Code attribute" rather than as a missing-feature error.

## Why it matters — it is what makes netty's HotSpot-25 code path unusable

netty 4.2 allocates direct memory through `CleanerJava25`, which is an
FFM-`Arena`-backed allocator reached by `MethodHandle`. On JDK 25 that is
netty's default whenever `PlatformDependent.hasUnsafe()` is false — which is
HotSpot 25's default. Measured end to end:

```
                                       hasUnsafe  CleanerJava25.isSupported  new AdaptiveByteBufAllocator().directBuffer(256)
HotSpot JDK 25                         false      true                       ok, cap=256
CratonVM -Dio.netty.noUnsafe=true      false      true                       IllegalStateException: Unexpected allocation exception
                                                                               CAUSE: AbstractMethodError: MemorySegment.asByteBuffer() has no Code attribute
CratonVM (default)                     true       true                       ok, cap=256
```

CratonVM's default only works because it pins
`sun.misc.unsafe.memory.access=allow`, which keeps netty on its `sun.misc.Unsafe`
path and away from `CleanerJava25` entirely — see
[unsafe-memory-access-property-flips-netty-to-unsafe-paths](unsafe-memory-access-property-flips-netty-to-unsafe-paths-20260812.md).

**This is the reason that property cannot simply be removed.** Forcing netty
onto HotSpot 25's actual default path with `-Dio.netty.noUnsafe=true` takes
`io.netty.buffer.AdaptiveByteBufAllocatorTest` from 126/127 passing to
**17/127**, with 109 failures, every one of them this
`AbstractMethodError`. Reproduced twice, identical both times (ABBA
interleaved, same box, same binary):

| arm | wall | ok | failed |
| --- | --- | --- | --- |
| default (`hasUnsafe=true`) | 535 s / 500 s | 126 | 0 |
| `-Dio.netty.noUnsafe=true` | 77 s / 77 s | **17** | **109** |

The wall-time difference in that table is **not** a speedup and must not be
read as one: arm B exits 109 of 127 tests early on this very defect, so it does
far less work. That misreading is retracted in
[adaptive-bytebuf-allocator-throughput](adaptive-bytebuf-allocator-throughput-20260812.md),
which re-measured the gap with a census and `perf`. What the table *is* good
for is this page's own point — the failure is 100% reproducible and single-cause.

## Scope

Anything that takes a `ByteBuffer` view of an FFM segment is affected, not just
netty: this is the standard bridge between the `java.lang.foreign` API and
every existing NIO-based API, and it is the JDK 25 replacement for
`Unsafe.allocateMemory` + `DirectByteBuffer`. Expect more libraries to land on
it as they drop `sun.misc.Unsafe`.

## Suggested fix

Register a native for `MemorySegment.asByteBuffer()` alongside the existing
segment natives in `native-builtins/src/phases_late/foreign_ffm.rs`: mint a
direct `java.nio.ByteBuffer` whose address/capacity are the segment's, the same
way `dbb_allocate` builds one, and keep the segment reachable for the
buffer's lifetime so the arena's memory is not reclaimed underneath it.
Bounds and `isReadOnly` should follow the segment's own.

Worth doing together with an audit of which other `MemorySegment` /
`MemoryLayout` / `Arena` interface methods have no native — the "no Code
attribute" shape means every one of them fails at the call site with an error
that names dispatch rather than the missing feature, so they will only be found
one application at a time otherwise.

## Repro

```bash
CP=$(sed -n 2p apps/netty-suite-runner/common.args)   # any classpath with netty-common
cat > CleanProbe2.java <<'EOF'
import io.netty.buffer.AdaptiveByteBufAllocator;
import io.netty.util.internal.PlatformDependent;
public class CleanProbe2 {
    public static void main(String[] a) {
        System.out.println("hasUnsafe=" + PlatformDependent.hasUnsafe());
        try {
            System.out.println("cap=" + new AdaptiveByteBufAllocator().directBuffer(256).capacity());
        } catch (Throwable t) {
            System.out.println("FAILED: " + t);
            for (Throwable c = t.getCause(); c != null; c = c.getCause()) System.out.println("  CAUSE: " + c);
        }
    }
}
EOF
javac -cp "$CP" -d . CleanProbe2.java
cratonvm --java-home <jdk25> -Dio.netty.noUnsafe=true -cp "$CP:." CleanProbe2
```
