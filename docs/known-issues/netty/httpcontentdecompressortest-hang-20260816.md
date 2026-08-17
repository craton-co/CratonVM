# `HttpContentDecompressorTest` — `testZipBomb` moves 256 MiB through `ByteBuffer` accessors that cost ~260 ns each

**Status: OPEN, throughput. Diagnosed 2026-08-17** (was: observed but
undiagnosed, 2026-08-16). Original measurement 2026-08-16 on commit
`3ef3eb744`; diagnosis 2026-08-17 on `cf141b8a8`, Windows host, release build,
G1, real-JDK mode, cross-checked against HotSpot 25 on the same host.

## Summary

| | found | ok | failed | wall |
|---|---|---|---|---|
| CratonVM G1 (isolated) | 0 | 0 | 0 | **HANG, rc=124 @ 180s** |
| HotSpot 25 (isolated) | 8 | 8 | 0 | 11.8s |

## It is one test, and it is slow rather than stuck

The 2026-08-16 page could not say which of the four tests the process was
inside, because the harness prints a line only for a *failing* test. Running
the class under a per-test-progress launcher (`@@BEGIN` / `@@END` around every
individual test) answers it in one run:

```
@@BEGIN  ...testZipBomb(java.lang.String)/[test-template-invocation:#1]
@@END    FAILED 193144ms ...#1
         java.util.concurrent.TimeoutException: testZipBomb timed out after 120 seconds
@@BEGIN  ...testZipBomb(java.lang.String)/[test-template-invocation:#2]
```

Parameterization **#1 is `gzip`**. It takes 193 s, and JUnit's 120 s default
timeout *does* fire on it; the harness's 180 s process cap then kills the run
while `#2` (`deflate`) is starting. So this is not a hang and not a livelock —
it is throughput, and the class needs five such parameterizations plus three
other tests.

## Where the 193 s goes

`probes/NettyZipBombPhases.java` runs `testZipBomb`'s phases with a settable
chunk count. Marginal cost per 1 MiB chunk:

| phase | HotSpot | CratonVM | ratio |
|---|---|---|---|
| compress (`writeOutbound` of one 1 MiB `HttpContent`) | 4.3 ms/MiB | **345 ms/MiB** | 80x |
| decompress | ~0.2 ms/MiB | ~15 ms/MiB | 75x |

Splitting the per-chunk work further (alloc / fill / pipeline write):

| | HotSpot | CratonVM |
|---|---|---|
| `alloc.buffer(1 MiB)` | 2.8 ms/MiB | 1.3 ms/MiB |
| **`buffer.writeZero(1 MiB)`** | **0.3 ms/MiB** | **534 ms/MiB** |
| `ch.writeOutbound(...)` | 5.7 ms/MiB | 14.0 ms/MiB |

`writeZero` is the whole cost, and it is **1780x**. It is not a codec problem —
the zlib primitives are within 1.3x (`CRC32.update` 0.37 vs 0.04 ms/MiB,
`Deflater.deflate` 5.42 vs 4.05 ms/MiB, `Deflater` SYNC_FLUSH loop 5.88 vs
4.28 ms/MiB).

## `writeZero` is a loop of `ByteBuffer` accessors, and each one is a native call

`AbstractByteBuf.writeZero(int)` is `length >>> 3` calls to `_setLong`, i.e.
**131 072 per MiB**, and the test moves 256 MiB — 33.5 million of them per
parameterization. netty selects its *non-Unsafe* `PooledDirectByteBuf` here
(see "What was ruled out"), whose `_setLong` is
`java.nio.ByteBuffer.putLong(int, long)`, which CratonVM services with a
registered native.

`probes/NioAccessorRate.java`, same host:

| op | HotSpot | CratonVM | ratio |
|---|---|---|---|
| `byte[]` store | 0.16 ns | 4.53 ns | 28x |
| `ByteBuffer.put(int,byte)` direct | 0.29 ns | **282 ns** | 970x |
| `ByteBuffer.put(int,byte)` heap | 0.45 ns | 221 ns | 490x |
| `ByteBuffer.putInt(int,int)` direct | — | ~900 ns | — |
| **`ByteBuffer.putLong(int,long)` direct** | 0.30 ns | **1088 ns** | **3600x** |
| **`ByteBuffer.putLong(int,long)` heap** | 0.28 ns | **1058 ns** | **3800x** |
| `ByteBuffer.getLong(int)` direct | 0.49 ns | 1430 ns | 2900x |

131 072 x ~1090 ns is 143 ms/MiB from `putLong` alone; the `EmbeddedChannel`
allocator's buffer measured 534 ms/MiB — the same shape at a different buffer
kind. Either way 256 MiB of it does not fit in 180 s.

**A single-byte `put` already costs ~260-280 ns** — one native call, one stored
byte. That per-call floor, not anything about the buffer, is the finding.

## What was ruled out, with the measurement that ruled it out

* **Per-byte storage re-resolution.** `s2_bb_write8` / `s2_bb_read8`
  (`native-builtins/src/servlet.rs`) really did call `s2_bb_put_byte` /
  `s2_bb_get_byte` once per byte, and each of those re-resolved the backing
  store from scratch through up to three NAME-keyed field lookups (`hb`,
  `offset`, `address`) — 8x redundant work per `putLong`. It looked like the
  answer. It is not: rewriting all six accessors to resolve storage ONCE
  (`s2_bb_read_n` / `s2_bb_write_n`, landed 2026-08-17) moved `direct putLong`
  from 918 to 943 and from 864 to 877 ns/op, interleaved, two rounds — nothing.
  The `putLong`-to-`put(byte)` ratio is ~3.5x, not 8x, which is the shape of one
  native call plus a few field reads rather than eight byte stores. The rewrite
  is kept (strictly less work, pinned by `probes/NioAccessorOracle.java`) but it
  is **not** a fix for this page.
* **netty refusing `sun.misc.Unsafe`.** netty does select the non-Unsafe
  `PooledDirectByteBuf` on CratonVM, and `-Dio.netty.noUnsafe=false` cuts the
  compress phase 11499 -> 1735 ms (6.6x). But **HotSpot 25 reports the identical
  `hasUnsafe()=false` with the identical cause** — "sun.misc.Unsafe: unavailable
  (io.netty.noUnsafe=true by default on Java 25+)". That is netty's own Java-25
  policy on both VMs, so the non-Unsafe path is the path HotSpot also takes, and
  HotSpot still runs `writeZero` at 0.3 ms/MiB. Not a CratonVM defect, and not a
  legitimate accommodation either.
* **The compression codec.** See the zlib table above — 1.3x.
* **Leak detection / `refCnt` on this path.** netty's own
  `-Dio.netty.buffer.checkAccessible=false` drops `ByteBuf.setByte` from 3065 to
  668 ns, so the `ensureAccessible()` -> `refCnt()` ->
  `AtomicIntegerFieldUpdater.get` chain *is* ~2400 ns per checked accessor
  (`AIFU.get` alone measures 701 ns against HotSpot's 0.21, and
  `AIFU.compareAndSet` 752 ns against 4.66). But `writeZero` uses the unchecked
  `_setLong`, and its cost was **unchanged** by that switch — 1097 / 1068 /
  1101 ns across baseline, `checkAccessible=false`, and `+checkBounds=false`.
  `refCnt` is a real and large defect for every *checked* netty accessor; it is
  just not this page's.

## What would fix it

The per-call native floor. Two in-tree data points size the prize, measured in
one process and in both compile doors: `AtomicInteger.getAndIncrement`, which
has a JIT intrinsic, costs **5.3 ns**; `AtomicInteger.get`, which does not,
costs **160 ns**. The same funnel prices every other trivial accessor —
`sun.misc.Unsafe.getInt` 573 ns, `Unsafe.getIntVolatile` 614 ns,
`Enum.ordinal` 222 ns, `Object.getClass` 220 ns, `Object.equals` 190 ns,
`Object.hashCode` 173 ns — all registered natives whose real JDK bodies are one
or two bytecodes.

Bringing `ByteBuffer`'s absolute accessors onto that intrinsic ladder, or onto
the thin `*_DIRECT_FN` helper pattern already used for `Integer.valueOf`,
`Integer.intValue`, the two `HashMap` fast paths and `Thread.currentThread`, is
what takes `writeZero` from 534 ms/MiB to something that fits: at the intrinsic
rate the 33.5 M `putLong` calls per parameterization cost ~0.2 s instead of
~36 s.

Note the scope caveat recorded at the IR direct-call site in `jit/src/lib.rs`:
those six existing helpers are still single-pass-only, and wiring one into the
optimizing tier "changes what the optimizing tier emits on a measured hot path".
Any such addition needs an interleaved A/B at both doors.

## Repro

```bash
cd apps/netty-suite-runner
printf 'io.netty.handler.codec.http.HttpContentDecompressorTest\n' > /tmp/one.txt
./run-netty-suite.sh --list /tmp/one.txt --gc g1 --shards 1 --out runs/repro
./run-netty-suite.sh --list /tmp/one.txt --hotspot --shards 1 --out runs/repro
```

The three probes that carry the numbers above (compile against the suite
classpath in `apps/netty-suite-runner/cp-javac.args`):

```bash
cratonvm --java-home <jdk> @common.args NettyZipBombPhases gzip 32
cratonvm --java-home <jdk> @common.args NioAccessorRate 4000000 40
cratonvm --java-home <jdk> -cp . NioAccessorOracle   # must print HotSpot's TOTAL exactly
```

## Related

* `httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md`,
  `httpresponsestatustest-exhaustive-loop-timeout-20260816.md` — the other two
  `codec-http` walls from the same batch. Different mechanism: those are
  compiled-code call cost, this one is native-call cost.
* `adaptive-bytebuf-allocator-throughput-20260812.md` — the same per-entry
  transfer machinery, reached from a different netty class.
