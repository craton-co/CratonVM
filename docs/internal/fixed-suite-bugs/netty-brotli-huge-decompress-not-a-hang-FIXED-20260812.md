# `BrotliIntegrationTest` — not a hang, and not outside the interpreter

**Status:** ✅ DIAGNOSED AND CLOSED (2026-08-12). The original page's title was
wrong in both halves and its "leading hypothesis" pointed at the wrong
subsystem; everything below replaces it with measurements. The one **fixable**
defect it exposed — the watchdog telling you a running thread was blocked in
native code — is fixed in
`vm/src/threading/thread_registry.rs` + `vm-cli/src/main.rs`. What remains is a
known throughput wall that is **not specific to brotli, netty or this class**,
tracked where it belongs.

Was: the one HANG on [netty investigate-batch-04](investigate-batch-04.md).

## What the page used to say, and what is actually true

| the page said | measured |
|---|---|
| "hangs **before any test runs**" | **10 of the 11 tests pass in 4.2 s.** Only `testHugeDecompress` does not finish. |
| "the VM stops producing output entirely" | It stops producing output because the last test to start prints nothing until it ends. The process is at **100% of one core** the whole time. |
| "the watchdog never fires" | It fires and aborts (`rc=134`). It just produced **no live dump for the running thread**, which is a different thing. |
| "blocked **outside the interpreter** — in a native call, a library load, or a lock" | Not blocked at all. `/proc/<pid>/status` says the `main-vm` thread is **`R` (running)**, `utime` climbs ~100 ticks/s, and its native stack is `jit_invoke_virtual_mic` → `try_jit_site_cached_native_dispatch` — i.e. **JIT-compiled Java**. |
| "leading hypothesis: brotli4j's `System.loadLibrary`/JNI" | The library loads fine. A standalone probe replaying `Brotli4jLoader.<clinit>` step by step reaches `System.load(/tmp/.../libbrotli.so)` in **31 ms** and reports `isAvailable=true`. |

The one observation the page got right — the watchdog cannot reach the thread —
is real, and it is the whole reason the rest went wrong. See below.

## What the test actually does

`AbstractIntegrationTest.testHugeDecompress` builds **256 MB** one byte at a
time before any compression happens:

```java
int chunkSize = 1024 * 1024, numberOfChunks = 256;
for (int i = 0; i <= numberOfChunks; i++) {
    ByteBuf in = compressChannel.alloc().buffer(chunkSize);
    for (int j = 0; j < chunkSize; j++) {
        byte byteValue = (byte) (i + (j & 0xA0));
        in.writeByte(byteValue);      // <- 268,435,456 times
        digest.update(byteValue);     // <- 268,435,456 times
    }
    ...
}
```

Per-test timings, same host, same classpath (`ProgressRunner`, a CratonRunner
that logs `START`/`FINISH` per test — the stock runner prints nothing until the
class ends, which is why "no `@@RESULT`" was read as "no test ran"):

```
HotSpot JDK 25 : 10 tests done by 1.7 s, testHugeDecompress at 8.8 s, class 8.8 s
CratonVM       : 10 tests done by 4.2 s, testHugeDecompress never finishes
```

## The wall, measured

`HugeLoopProbe` — the three components of that inner loop, each in its own
method so the JIT compiles them separately, extrapolated to the 268 M
iterations the test performs:

| loop | HotSpot | CratonVM | full 268 M loop on CratonVM |
|---|---|---|---|
| arithmetic only (control) | 0.4 ns | 2.2 ns | 0.6 s |
| `MessageDigest.update(byte)` | 6.6 ns | 179 ns (27x) | 48 s |
| `ByteBuf.writeByte` | 7–10 ns | **2 500–2 800 ns (≈300x)** | **750 s** |
| both, as the test runs them | 10.0 ns | 3 054 ns | 820 s |

820 s to *build* the data, before a byte is compressed, against a 180 s suite
wall. That is the whole story.

`MessageDigest.update(B)V` is a registered native (2.6 M invocations in the
probe run) at ~179 ns — ordinary native-call cost, and only 6% of the problem.

### The 300x is `ByteBuf`, and it is not the `Unsafe` path

The obvious suspect was
[netty taking the `Unsafe` path here and not on HotSpot](unsafe-memory-access-property-flips-netty-to-unsafe-paths-20260812.md)
— CratonVM pins `sun.misc.unsafe.memory.access=allow`, so `hasUnsafe()` is
`true` here and `false` on HotSpot 25, and the allocator hands out
`PooledUnsafeHeapByteBuf` instead of `PooledHeapByteBuf`. **It is not that.**
Turning it off changes almost nothing:

| ns per `writeByte` | HotSpot | CratonVM |
|---|---|---|
| default (`hasUnsafe` true here / false there) | 9.2 | 2 695 |
| `-Dio.netty.noUnsafe=true` (same class on both) | 9.2 | **1 943** |
| raw `byte[] a[j] = v` on the same VM | ~0 (elided) | 2.0–4.5 |

Every flavour is 200–300x: pooled/unpooled, heap/direct, `writeByte`/`setByte`.
A raw array store is 2 ns. So the cost is in the *call chain* —
`AbstractByteBuf.writeByte` → `ensureWritable0` → `_setByte` →
`HeapByteBufUtil`/`UnsafeByteBufUtil` → `PlatformDependent(0)` — none of which
is a native: `--dump-native-registry` shows **no native at all** in that loop
(top entry is `Object.<init>` at 14 591 calls against ~650 000 `writeByte`s).

What it *is*: `CRATONVM_DBG_JIT_SCAN_PROF=1` reports **`jit_entries=2 980 692`**
for ~1.4 M loop iterations — **~2 compiled-code entries per byte written**. The
chain is not inlined, so every byte pays two JIT↔interpreter transitions at the
300–700 ns each that
[`jit-entries-per-call-cost`](../performance/vm-per-call-dispatch-cost-RETIRED-20260813.md)
prices. `--nojit` gives 9 415 ns for the same call, so the JIT *is* helping
(3.6x) — it just cannot inline through it, where the same JIT gives 31x on a
plain array store.

**This is not a brotli, netty or `BrotliIntegrationTest` property.** Any
per-byte `ByteBuf` loop pays it. It belongs to the ByteBuf-throughput
workstream —
[netty per-call throughput](../performance/netty-per-call-throughput-20260813.md)
— and this page should not be the place anyone looks for it.

## The defect that was fixed: the watchdog blamed native code for compiled code

`--stack-dump-on-timeout` arms a watchdog whose dump hook lives in **the
interpreter dispatch loop**. A thread executing JIT-compiled code never reaches
it, so it never acks and no `T19.H1 stack dump: tid=N` section is emitted for
it. Two pieces of output then actively misled the reader:

* the per-thread summary tagged the thread `deposit=STALE` and told you *"its
  real position is in the `T19.H1 stack dump: tid=0` section above"* — **a
  section that was never emitted.** Its absence read as "the watchdog cannot
  reach it", which reads as "it is blocked somewhere else";
* when nothing acked at all, the watchdog printed *"no Java threads responded —
  **main thread is in native (Rust) code**"*. A guess, stated as a fact, and in
  this case the wrong one of the two possibilities.

The summary is now printed **after** the grace period rather than at request
time (only then is it known who answered), each thread id that dumps is
recorded, and a RUNNING thread that produced no dump is labelled:

```
tid=0 ... alive=true blocked=false ... deposit=STALE top=...
      ^ no-live-dump: this thread produced NO "T19.H1 stack dump: tid=0" section
        — it never reached an interpreter dispatch point, so it is in
        JIT-compiled code or a long native call. Re-run with --nojit; if the
        live dump appears there, it was compiled code (and it was RUNNING, not
        stuck).
```

The zero-ack message no longer asserts native code; it names both states and
the flag that separates them. And the advice works — on this exact run:

```
default : 1 thread dumped, tid=0 flagged no-live-dump
--nojit : 3 dump sections, tid=0 has 76 frames, deepest frame =
          io/netty/buffer/AbstractByteBuf.writeByte(I)Lio/netty/buffer/ByteBuf;
          inside AbstractIntegrationTest.testHugeDecompress
```

Two minutes, no gdb, no guessing — against the several hours the original page
represents.

## How to re-check this class

```bash
cd apps/netty-suite-runner
# per-test progress; the stock runner cannot tell you WHICH test it died in
<cv-bin> --java-home "$JAVA_HOME" --Xmx 1500m @common.args -Dcraton.batch=1 \
    ProgressRunner io.netty.handler.codec.compression.BrotliIntegrationTest
# expect: 10 FINISH ... SUCCESSFUL by ~4 s, then START testHugeDecompress
```

`testHugeDecompress` will finish when a per-byte `ByteBuf.writeByte` costs on
the order of 20 ns instead of 2 500 — i.e. when the ByteBuf call chain inlines.
Nothing else about this class needs attention: the other ten tests pass, and
they passed before this investigation started.
