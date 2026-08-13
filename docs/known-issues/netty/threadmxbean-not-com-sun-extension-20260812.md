# `ManagementFactory.getThreadMXBean()` does not return a `com.sun.management.ThreadMXBean`

**Status:** OPEN (2026-08-12). Found while triaging
[netty investigate-batch-01](investigate-batch-01.md) on the Azure Linux host
(`20.80.105.49`), binary built from `origin/dev` `1c4ce7d3a`.

## Symptom

`io.netty.buffer.AdaptiveByteBufAllocatorTest.shouldReuseChunks()` is the one
test in that class CratonVM does not run. It is skipped, not failed:

```
org.opentest4j.TestAbortedException: assumption was not met due to:
Expecting actual:
  java.lang.management.ThreadMXBean@cf71
to be an instance of:
  com.sun.management.ThreadMXBean
but was instance of:
  ...
```

HotSpot JDK 25 runs it (`ok=127`); CratonVM reports `ok=126 aborted=1`. Same
shape, smaller, as the `AlignedPooledByteBufAllocatorTest` divergence in
unsafe-memory-access-property-flips-netty-to-unsafe-paths (retired: `unsafe-memory-access-property-flips-netty-to-unsafe-paths-20260812`)
— a `Assumptions.assumeTrue`/`assertThat(...).isInstanceOf(...)` guard whose
answer differs, so a different *set* of tests runs on each VM.

On a real JVM `ManagementFactory.getThreadMXBean()` returns an object that
implements the platform-extension interface `com.sun.management.ThreadMXBean`
(which adds `getThreadAllocatedBytes`, `getCurrentThreadAllocatedBytes`,
`getThreadCpuTime` variants, …). CratonVM returns something that satisfies only
the `java.lang.management.ThreadMXBean` base interface.

Probed directly — and `getOperatingSystemMXBean()` has the identical shape:

| | HotSpot JDK 25 | CratonVM |
| --- | --- | --- |
| `getThreadMXBean().getClass()` | `com.sun.management.internal.HotSpotThreadImpl` | `java.lang.management.ThreadMXBean` |
| `instanceof com.sun.management.ThreadMXBean` | `true` | **`false`** |
| `getOperatingSystemMXBean().getClass()` | `com.sun.management.internal.OperatingSystemImpl` | `java.lang.management.OperatingSystemMXBean` |
| `instanceof com.sun.management.OperatingSystemMXBean` | `true` | **`false`** |

Both beans are fabricated as instances of the *interface* they are typed by, so
they declare no interfaces of their own and fail every type test — including
the base-interface one a caller might reasonably rely on. `com.sun.management.
OperatingSystemMXBean` is the standard source of process/system CPU load and
physical-memory figures, so the same silent-fallback risk applies there.

## Impact

Low on its own — one skipped test — but the *class* of problem is not: any
library that feature-detects a platform MXBean extension and falls back
silently will take its fallback path on CratonVM. Allocation-rate and CPU-time
accounting (netty's chunk-reuse heuristics here, and most JVM-profiling
libraries) are the common consumers. The failure mode is a silently different
code path, not an exception, so it does not show up as a test failure anywhere
it is guarded.

## Suggested fix

Have the synthetic `ThreadMXBean` declare `com.sun.management.ThreadMXBean`
among its interfaces (and `OperatingSystemMXBean` its `com.sun` counterpart)
and implement at least
`getThreadAllocatedBytes(long)` / `getCurrentThreadAllocatedBytes()` /
`isThreadAllocatedMemorySupported()` / `getThreadCpuTime(long[])`. A stand-in
that declares no interfaces fails every `instanceof`/`isInstanceOf` test
against it — the same trap recorded for fabricated stand-ins generally.

If the allocation counters cannot be sourced, returning `-1` from the getters
and `false` from `isThreadAllocatedMemorySupported()` is still better than
failing the type test: that is the documented "not supported" answer and
callers already handle it, whereas the type mismatch is not something the JDK
contract lets them expect.

## Repro

```java
import java.lang.management.ManagementFactory;
public class MxProbe {
    public static void main(String[] a) {
        Object b = ManagementFactory.getThreadMXBean();
        System.out.println(b.getClass().getName());
        System.out.println("isComSun=" + (b instanceof com.sun.management.ThreadMXBean));
    }
}
```
