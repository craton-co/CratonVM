# CratonVM pins `sun.misc.unsafe.memory.access=allow`, which flips netty onto its Unsafe fast paths where HotSpot 25 disables them

**Status:** OPEN (2026-08-12). Found while triaging
[netty investigate-batch-01](investigate-batch-01.md) on the Azure Linux host
(`20.80.105.49`), binary built from `origin/dev` `1c4ce7d3a`.

## Symptom

`io.netty.buffer.AlignedPooledByteBufAllocatorTest` runs a different *set* of
tests on CratonVM than on HotSpot JDK 25 — not a failure, a divergence:

| | HotSpot JDK 25 | CratonVM |
| --- | --- | --- |
| `found` | 49 | 49 |
| `ok` | 21 | 48 |
| `aborted` (failed assumption) | 28 | 1 |

Its `newAllocator()` opens with
`assumeTrue(PooledByteBufAllocator.isDirectMemoryCacheAlignmentSupported())`,
which is false on HotSpot and true on CratonVM.

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

| probe | HotSpot JDK 25 | CratonVM |
| --- | --- | --- |
| `System.getProperty("sun.misc.unsafe.memory.access")` | `null` | **`allow`** |
| `System.getProperty("io.netty.noUnsafe")` | `null` | `null` |
| `java.specification.version` | `25` | `25` |
| `PlatformDependent0.javaVersion()` | `25` | `25` |
| `PlatformDependent.getUnsafeUnavailabilityCause()` | `UnsupportedOperationException: sun.misc.Unsafe: unavailable (io.netty.noUnsafe=true by default on Java 25+)` | `null` |
| `PlatformDependent.hasUnsafe()` | `false` | **`true`** |

Every other input agrees. The property alone decides it.

HotSpot only sets `sun.misc.unsafe.memory.access` when the user passes
`--sun-misc-unsafe-memory-access=<mode>`; the JDK 25 default leaves it
**unset** (the effective mode is still "allow, with a warning"). CratonVM sets
it unconditionally in `vm/src/vm/vm_init.rs`.

## Why CratonVM sets it — and why it is not simply removable

The property is a deliberate workaround, documented in place: under the JDK 25
default (`warn`), the first legacy `sun.misc.Unsafe` memory access drops into
`Unsafe.beforeMemoryAccessSlow()`, which runs a `StackWalker.walk()` and
unconditionally dereferences `frames.get(1)`. CratonVM's `StackWalker` can
return fewer than two frames for some native/JIT-spliced call chains, so
`List.get(1)` throws `ArrayIndexOutOfBoundsException` — it surfaced in jctools'
`MpscUnboundedArrayQueue` (netty's per-`NioEventLoop` task queue) as "failed to
create a child event loop". Pinning the property to `allow` makes
`beforeMemoryAccess()` return at its first check and bypasses the warning
machinery entirely.

So the property is load-bearing today. Just deleting it re-opens the
`StackWalker`/`beforeMemoryAccessSlow` crash.

## It is also masking a much larger gap — measured

Forcing netty onto the path HotSpot 25 actually takes
(`-Dio.netty.noUnsafe=true`) does not merely change which tests are skipped; it
**breaks the allocator outright**. ABBA-interleaved, same box, same binary,
`io.netty.buffer.AdaptiveByteBufAllocatorTest`:

| arm | round 1 | round 2 | ok | failed |
| --- | --- | --- | --- | --- |
| A — default (`hasUnsafe=true`) | 535 s | 500 s | 126 | 0 |
| B — `-Dio.netty.noUnsafe=true` | 77 s | 77 s | **17** | **109** |

All 109 failures are one cause: netty's non-Unsafe allocator is
`CleanerJava25`, an FFM-`Arena`-backed one, and CratonVM has no
`MemorySegment.asByteBuffer()` →
[memorysegment-asbytebuffer-unimplemented](memorysegment-asbytebuffer-unimplemented-20260812.md).

Two consequences:

* **Do not "fix" this by dropping the property until the FFM gap is closed.**
  Matching HotSpot's property set today would take netty from mostly-green to
  mostly-red. The property is not just a `StackWalker` workaround any more; it
  is what keeps netty off an unimplemented path.
* Arm B being **7× faster** on identical Java work is its own finding about
  CratonVM's Unsafe natives →
  [adaptive-bytebuf-allocator-throughput](adaptive-bytebuf-allocator-throughput-20260812.md).

## Impact

Narrow in *this* batch (one class, and the extra 28 tests all pass), but the
blast radius is the whole netty suite and every other Unsafe-aware library:
netty exercises its `PlatformDependent0` Unsafe fast paths on CratonVM
(direct-buffer address arithmetic, `UnsafeByteBufUtil`, `setMemory`,
`copyMemory`, unaligned access, the `*Unsafe*ByteBufTest` families) while a
HotSpot JDK 25 baseline for the same suite exercises the `ByteBuffer`/
`MemorySegment` safe paths. Any "CratonVM vs HotSpot" comparison over netty is
therefore comparing two different netty code paths, not two VMs running the
same code — that is worth knowing before triaging any further batch page.

It also means CratonVM's Unsafe implementation is on the hot path for every
netty buffer operation, so a defect there shows up as a netty buffer bug.

## Suggested fix — ordered, and the order matters

1. **First close the FFM gap**
   ([`MemorySegment.asByteBuffer()`](memorysegment-asbytebuffer-unimplemented-20260812.md)),
   so netty's non-Unsafe path works at all. Nothing below is safe before this.
2. **Then fix the underlying `StackWalker` shortfall** so
   `beforeMemoryAccessSlow()` sees ≥ 2 frames, and stop setting the property —
   or set it only when the user passes `--sun-misc-unsafe-memory-access`. This
   is the only option that makes CratonVM's observable property set match
   HotSpot's.
3. Alternatively, keep the internal effect but stop *publishing* it: leave
   `System.getProperty("sun.misc.unsafe.memory.access")` unset and drive
   `Unsafe.MEMORY_ACCESS_OPTION` from a VM-internal default instead. The
   post-clinit fixup in `vm/src/vm/vm_util.rs` already writes that static
   directly, so the property read there would become an override rather than
   the source. Cheaper than (2), and it fixes the observable divergence — but
   it silently changes which path netty and every other Unsafe-aware library
   takes, so it still needs (1) first.

Whichever route, cross-check the netty suite and Keycloak before and after —
this changes which code path a large amount of third-party code runs.

## Repro

```bash
# on the Azure Linux host, with netty's test classpath
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
java     -cp "$CP:." UnsafeWhy2          # prop=[null]  hasUnsafe=false
cratonvm --java-home <jdk25> -cp "$CP:." UnsafeWhy2   # prop=[allow] hasUnsafe=true
```
