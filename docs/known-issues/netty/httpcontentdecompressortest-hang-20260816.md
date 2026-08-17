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

## One `putLong` is SEVEN native calls

`--dump-native-registry` reports a per-native invocation count, so the question
"what does one accessor actually execute" is answerable without a profiler.
Running `NioAccessorRate` (800 000 ops per arm) and dumping:

| invocations | native | registered by |
|---:|---|---|
| 4 000 000 | `jdk/internal/util/Preconditions.checkIndex(IILjava/util/function/BiFunction;)I` | `native-builtins/src/preconditions.rs:404` |
| 3 200 000 | `java/lang/ref/Reference.reachabilityFence(Ljava/lang/Object;)V` | `native-builtins/src/lib.rs:14012` |
| 2 400 000 | `java/nio/DirectByteBuffer.session()Ljdk/internal/foreign/MemorySessionImpl;` | `native-builtins/src/lib.rs:19560` |
| 1 600 000 | `jdk/internal/misc/ScopedMemoryAccess.putLongUnaligned(...)` | `native-builtins/src/lib.rs:15697` |
| 800 000 | `java/nio/DirectByteBuffer.put(IB)Ljava/nio/ByteBuffer;` | `native-io/src/direct_buffer.rs:1908` |
| 800 000 | `java/nio/HeapByteBuffer.session()...` | `native-builtins/src/lib.rs:19560` |
| 800 000 | `ScopedMemoryAccess.getLongUnaligned(...)` / `putIntUnaligned(...)` | `native-builtins/src/lib.rs:15679` |

That is **~7 native calls for one `ByteBuffer.putLong(int,long)`**, at the
~160 ns funnel cost each — which is where 1088 ns comes from, arithmetic that
closes.

And **three of the four hottest are trivial or literally constant**:

* `Reference.reachabilityFence` is `black_box(arg); Ok(None)` — a no-op. HotSpot
  intrinsifies it to *nothing at all*. 3.2 M calls, ~2 per accessor.
* `DirectByteBuffer.session()` is `Ok(Some(Value::Object(None)))` — it returns
  the constant `null`. 2.4 M calls.
* `Preconditions.checkIndex(int,int,BiFunction)` is
  `if (index < 0 || index >= length) throw; return index;`. 4 M calls.

Together they are 9.6 M of the ~11.2 M native calls in that run. `put(int,byte)`
at ~260 ns is the same story with fewer rungs.

**A single-byte `put` already costs ~260-280 ns** — one native call, one stored
byte. That per-call floor, multiplied by the rung count above, is the finding.

## Two of the four rungs are FIXED (2026-08-17); the class is still over

`Preconditions.checkIndex` and `Reference.reachabilityFence` are now bound to
thin `*_DIRECT_FN` helpers (`jit_preconditions_check_index_direct`,
`jit_reachability_fence_direct`) instead of going through the generic native
funnel. Measured on `probes/HotNativeRungRate.java`, same host, HotSpot 25 for
scale:

Measured as a SAME-BINARY A/B on the kill switch
(`CRATONVM_JIT='-census-direct-helpers'`, default on), which is the only form
of this comparison that is trustworthy — see "A cross-binary A/B is not an A/B"
below:

| rung | HotSpot | helpers off | helpers on | ratio |
|---|---|---|---|---|
| `Objects.checkIndex` (-> `Preconditions.checkIndex`) | 0.27 ns | 143.58 ns | **23.71 ns** | **6.1x** |
| `Reference.reachabilityFence` | 0.28 ns | 150.73 ns | **23.51 ns** | **6.4x** |

The invocation census confirms it is the bind and not the timing:
`Preconditions.checkIndex` 4 000 000 -> **1 174** invocations,
`Reference.reachabilityFence` 3 200 000 -> **163 090**; and the bind counter
goes `checkIndex=2 reachabilityFence=2` to `0 0` with the switch.

### A cross-binary A/B is not an A/B

The first numbers taken for this section were 352 ns and 361 ns "before", giving
18x and 19x. They were measured against the binary in the main worktree, which
was **a day older than the branch** — so they carried every unrelated change
that landed on `dev` in between, and they overstate the effect by ~2.5x. The
same mistake showed up much more loudly on CratonBench, where that pairing
reported ~13-19% "regressions" on `arithmetic` and `fib` — phases that contain
no `checkIndex` and no `reachabilityFence` call at all, so the binds cannot
have caused them.

The kill switch was added for exactly this reason: one binary, one gate. The
6.1x/6.4x above are that measurement.

**THREE doors, and only the third one mattered for the fence.** Wiring the
single-pass ladder and the IR path left the counter at
`Preconditions.checkIndex=2 Reference.reachabilityFence=0` while the fence's
cost sat unchanged at 142 ns. `checkIndex` had landed anyway because it is
reached through `Objects.checkIndex`, a JDK method the method-entry door
compiles, so the bind happened inside the callee; `reachabilityFence` has no
such intermediary and a hot loop calls it directly — and a hot loop's body is
compiled by the **OSR door** in
`vm/src/runtime/interpreter/jit_bridge.rs::compile_osr_artifact`, which runs its
own callee-binding loop rather than `jit::try_compile`'s ladder. Only after
wiring that third door did the fence move 142 -> 18.88 ns.
`CRATONVM_DBG=jit-method-stats` now prints
`JIT thin direct-helper binds: ...` unconditionally, including at zero, so this
is a counter question rather than a timing question.

**CratonBench is unaffected, and the census says so without the clock.** With
the helpers on, CratonBench binds **zero** sites
(`Preconditions.checkIndex=0 Reference.reachabilityFence=0`) and neither native
is invoked in either arm — it has no call sites for them. Every phase's checksum
is identical across HotSpot, helpers-off and helpers-on. That matters because
the host's own spread on a FIXED configuration reached 74% on `arithmetic`
(7526 ms against 13088 ms, same binary, same flags), which is larger than any
per-phase effect a timing A/B could have claimed. On a loaded host the bind
counter and the invocation census answer "did this touch the workload" and the
clock does not.

**End to end, this did not retire the page.** Interleaved, two rounds,
`NettyZipBombPhases gzip 32`: compress 21389/15653 ms before against
18508/16580 ms after — inside the noise. `ByteBuffer` accessors improved
roughly 1.5-2.4x (direct `putLong` ~1400 -> ~1000 ns, heap `put(byte)` ~283 ->
~114 ns), which is what removing 2 rungs of ~6 predicts, and not enough.

**The census head has moved, and names the remaining work:**

| invocations (800 000 ops) | native |
|---:|---|
| 2 400 000 | `java/nio/DirectByteBuffer.session()Ljdk/internal/foreign/MemorySessionImpl;` |
| 1 600 000 | `jdk/internal/misc/ScopedMemoryAccess.putLongUnaligned(...)` |
| 800 000 | `ScopedMemoryAccess.getLongUnaligned` / `putIntUnaligned` |
| 800 000 | `java/nio/DirectByteBuffer.put(IB)Ljava/nio/ByteBuffer;` |

`session()` is the new number one and is `Ok(Some(Value::Object(None)))` — it
returns the constant `null` — but it is an **`invokevirtual`**, so it cannot use
the `invoke_kind == 3` bind these two used; it needs the guarded-virtual direct
call (`JitDirectCall::guard_class_id`) or a receiver-typed variant.
`ScopedMemoryAccess.*Unaligned` is the actual store and must stay a native, but
one native per accessor is the floor, and a thin helper would price it at ~15 ns
rather than ~160.

## What was ruled out, with the measurement that ruled it out

* **Per-byte storage re-resolution in `servlet.rs`.** `s2_bb_write8` /
  `s2_bb_read8` really did call `s2_bb_put_byte` / `s2_bb_get_byte` once per
  byte, each re-resolving the backing store through up to three NAME-keyed field
  lookups (`hb`, `offset`, `address`) — 8x redundant work per `putLong`. It
  looked like the answer. Rewriting all six accessors to resolve storage ONCE
  (`s2_bb_read_n` / `s2_bb_write_n`, landed 2026-08-17) moved `direct putLong`
  from 918 to 943 and from 864 to 877 ns/op, interleaved, two rounds — nothing.
  **The reason is that those natives are not on this path at all.** The
  invocation census above shows the real-JDK `DirectByteBuffer` / `HeapByteBuffer`
  bytecode running instead, served by `native-io/src/direct_buffer.rs` and the
  `ScopedMemoryAccess` / `session` / `Preconditions` / `reachabilityFence`
  natives; `java/nio/ByteBuffer.putLong` (the `servlet.rs` registration) records
  **zero** invocations in the probe. The rewrite is kept — it is strictly less
  work on the paths it *does* serve and it is pinned by
  `probes/NioAccessorOracle.java` — but it is **not** a fix for this page, and
  the census, not the microbenchmark, is what proved that. Ask
  `--dump-native-registry` which native actually serves a call before optimizing
  one.
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
