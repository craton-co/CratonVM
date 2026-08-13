# FIXED — `ManagementFactory` returned base-interface MXBeans, not the `com.sun` extensions

**Status:** ✅ FIXED 2026-08-13 on `fix/netty-tls-mxbean-20260812`. Retired from
`docs/known-issues/netty/threadmxbean-not-com-sun-extension-20260812.md`.

## What was wrong

`ManagementFactory.getThreadMXBean()` and `.getOperatingSystemMXBean()`
fabricated their beans as instances of the **base** interface they are typed
by, so every `instanceof com.sun.management.…` answered false:

| | HotSpot JDK 25 | CratonVM (before) | CratonVM (after) |
| --- | --- | --- | --- |
| `getThreadMXBean().getClass()` | `com.sun.management.internal.HotSpotThreadImpl` | `java.lang.management.ThreadMXBean` | `com.sun.management.ThreadMXBean` |
| `instanceof com.sun.management.ThreadMXBean` | `true` | **`false`** | `true` |
| `instanceof java.lang.management.ThreadMXBean` | `true` | `true` | `true` |
| `getOperatingSystemMXBean().getClass()` | `com.sun.management.internal.OperatingSystemImpl` | `java.lang.management.OperatingSystemMXBean` | `com.sun.management.OperatingSystemMXBean` |
| `instanceof com.sun.management.OperatingSystemMXBean` | `true` | **`false`** | `true` |

The failure mode was a silently different code path, not an exception: any
library that feature-detects a platform MXBean extension took its fallback
branch, and nothing anywhere reported a problem.

## The fix

**1. Allocate under the extension interface** —
`jmx.rs::alloc_extension_mxbean`. The extension interface *extends* the base
one, so the base `instanceof` and every `checkcast java/lang/management/…`
keep working. In synthetic-JDK mode there is no class file to carry that
relation, so `class_manager::jdk_interfaces` lists it explicitly; without that
entry the fix would have traded one broken type test for another.

**2. Register the bean's natives under both dispatch owners** — the receiver is
now typed `com.sun.management.…`, and an `invokeinterface` resolving through
that name must find a body rather than an abstract, Code-less entry.
`register_thread_mxbean` was split so the same surface is registered once per
owner; the `setThreadContentionMonitoringEnabled` loop, which names its own
owners, still runs once so no duplicate registration is filed.

**3. Implement the extension methods, with real numbers.**

`getThreadAllocatedBytes(long)` / `getCurrentThreadAllocatedBytes()` /
`getThreadAllocatedBytes(long[])` / `isThreadAllocatedMemorySupported()` /
`isThreadAllocatedMemoryEnabled()` / `setThreadAllocatedMemoryEnabled(boolean)`
/ `getTotalThreadAllocatedBytes()` / `getThreadCpuTime(long[])` /
`getThreadUserTime(long[])`.

The doc this replaces offered `-1` plus `isThreadAllocatedMemorySupported() ==
false` as an acceptable answer. That would have left netty's
`shouldReuseChunks` aborted at its `assumeTrue(allocBefore != -1)` — the same
skipped test, for a different reason. The counter is real instead:

* `Tlab::thread_allocated_bytes()` = a carried total plus the live
  `cursor - start` span. Reading the **live cursor** is what makes it
  JIT-aware: compiled code allocates through its own inline bump and tells no
  counter about it, so anything incremented at the interpreter's allocation
  sites would silently report a fraction of a compiled thread's allocation —
  and a caller asserting "we allocated less than N" reads that undercount as
  good news.
* `Tlab::note_external_allocation` covers what bypasses the TLAB entirely
  (humongous objects and arrays), in both the interpreter's
  `alloc_object_shared` / `gc_alloc_array` and the JIT's own copies of those
  slow paths.
* The running total is carried across a refill by
  `Tlab::adopt_allocation_total`, because `Tlab::new` builds a fresh struct.

Only the **calling** thread is measurable — the counter lives in that thread's
own TLAB, which no other thread may read while its owner is running — so
`getThreadAllocatedBytes(id)` answers for the caller's own id (and the JDK's
`0` == "current thread" convention) and returns the JMM's `-1` for any other.

**4. `com.sun.management.OperatingSystemMXBean`'s metrics are real on Linux.**
`getTotalMemorySize`, `getFreeMemorySize`, `getTotalSwapSpaceSize`,
`getFreeSwapSpaceSize`, `getCommittedVirtualMemorySize`, `getProcessCpuTime`,
`getOpenFileDescriptorCount`, `getMaxFileDescriptorCount` — read from `/proc`
by `jmx.rs::os_metrics`, which is now the single implementation behind the
interface methods, the JDK 8 `sun.management.OperatingSystemImpl` `*0` natives
and the JDK 9+ `com.sun.management.internal.OperatingSystemImpl` ones, so the
three surfaces cannot drift. They were a flat `-1`, justified by "CratonVM has
no portable in-VM source" — true of a *portable* source, and false of the
platform this VM is measured on, where the sibling `getSystemLoadAverage` had
been reading `/proc/loadavg` all along.

The two CPU-**load** doubles stay at `-1.0`. That is a different question, not
an unfinished half of the same one: a load is a fraction over an interval and
needs a previous sample, which this bean does not keep.

**5. `phases_late::management`'s duplicate factories now delegate** to
`jmx.rs`'s allocators. Per that module's own DISPATCH NOTE the `jmx.rs`
registration is the one that wins, so its copies were unreachable — and they
had already drifted twice over, allocating the base interface with **zero**
field slots where the count getters read six by index.

## Measured

`MxProbe` (`probes/`-style, in the session scratchpad), CratonVM vs HotSpot 25,
same host:

```
thread bean class = com.sun.management.ThreadMXBean
thread isComSun   = true          (was false)
thread isBase     = true
os isComSun       = true          (was false)
allocBefore = 1904
allocAfter  = 41945608 delta=41943704   (HotSpot: 41981464 — 0.09% apart)
isThreadAllocatedMemorySupported = true
os.getTotalMemorySize = 33651179520     (HotSpot: 33651179520 — identical)
```

`io.netty.buffer.AdaptiveByteBufAllocatorTest`:

| | before | after | HotSpot 25 |
|---|---|---|---|
| | `ok=126 aborted=1` | **`ok=127 failed=0 aborted=0`** | `ok=127` |

`shouldReuseChunks` is the test that was skipped. It now runs, and it passes on
the strength of a real allocation figure — it asserts the 100-iteration loop
allocates less than 8 MiB, which is only meaningful if the instrument can see
the 1 MiB-per-iteration allocations it would make without chunk reuse.

## Known residual

`SecretKeyFactory.getProvider()` on a factory built by
`getInstance(alg, Provider)` still reports `SunJCE` rather than the provider
that actually served it: the delegating accessor type-checks the real object's
`provider` field and falls back when the read does not produce a
`java.security.Provider`. The **algorithm** and the **derived key** are correct
(see the JCA provider-routing fix that landed alongside this), so this is a
cosmetic naming divergence, not a wrong answer.
