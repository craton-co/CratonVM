# `HttpContentDecompressorTest` — every test PASSES; the wall is `snappy`, and it was never the `ByteBuffer` accessors

**Status: OPEN on throughput, RE-DIAGNOSED 2026-08-18. The cause this page's
previous title named is CLOSED and was not the class's wall.** Three things
changed at once, so read them separately:

* the `ByteBuffer` absolute accessors this page blamed are **fixed** — a wide
  one was two native calls and is now one, measured 2.2-2.7x (§ "The
  accessors, fixed");
* the class no longer fails or hangs on its own terms — **all 8 tests report
  SUCCESSFUL** when JUnit's per-method budget is lifted (§ "Every test
  passes");
* and 88% of the class's 642 s is **one parameterisation, `snappy`**, whose
  cost is neither `writeZero` nor `ByteBuffer` (§ "The wall is snappy").

Measured 2026-08-18 on Azure host 2 (Linux x86_64, 8 cores, JDK 25), release
build, one binary per arm, load average recorded beside every number.
Cross-checked against HotSpot 25 on the same host and the same classpath.

## Every test passes

`probes/PerTestProgressRunner.java` prints `@@BEGIN` before each individual
test and `@@END` with its wall time, so a run killed at a cap still names what
was in flight. The whole class, real-JDK mode, engine-default collector,
`-Djunit.jupiter.execution.timeout.mode=disabled`, host load 3.9-6.5:

| test | ms | 2026-08-17 page |
|---|---:|---|
| `testZipBomb` #1 `gzip` | 18 739 | **193 144** |
| `testZipBomb` #2 `deflate` | 16 578 | not reached |
| `testZipBomb` #3 `br` | 18 535 | not reached |
| `testZipBomb` #4 `zstd` | 18 022 | not reached |
| **`testZipBomb` #5 `snappy`** | **566 348** | not reached |
| `testBrotliDecodingHonorsMaxAllocationAsOutputCap` | 3 531 | not reached |
| `testInvokeReadWhenNotProduceMessage` | 6 | not reached |
| `testFlowControlHandlerEmitsOneMessagePerRead` | 5 | not reached |
| **class total** | **642 322** | HANG @ 180 s |

Every one of the eight is `SUCCESSFUL`. There is no failure and no livelock in
this class — only a budget it exceeds, and 88% of that budget is `#5`.

`gzip` moving 193 s -> 18.7 s is `dev`'s accumulated work between 2026-08-17
and 2026-08-18, not anything on this page's branch; the arithmetic below shows
why this page's own fix could not have done it.

The class STILL exceeds the harness's cap — a 900 s run on 2026-08-18 with the
suite runner's default JUnit budget reported `HANG=1` — so this page stays
open. What it is open ON has changed completely.

## The accessors, fixed

### The census, re-taken

`--dump-native-registry` reports a per-native invocation count.
`probes/NioAccessorRate.java`, 1 600 000 operations per arm, current `dev`:

| invocations | native |
|---:|---|
| 4 800 000 | `java/nio/DirectByteBuffer.session()` |
| 3 200 000 | `jdk/internal/misc/ScopedMemoryAccess.putLongUnaligned(…)` |
| 1 600 000 | `java/nio/DirectByteBuffer.put(IB)Ljava/nio/ByteBuffer;` |
| 1 600 000 | `java/nio/HeapByteBuffer.session()` |
| 1 600 000 | `ScopedMemoryAccess.getLongUnaligned(…)` |
| 1 600 000 | `ScopedMemoryAccess.putIntUnaligned(…)` |
| 83 140 | `java/lang/ref/Reference.reachabilityFence(…)` |
| 1 187 | `jdk/internal/util/Preconditions.checkIndex(…)` |

The last two rows are the 2026-08-17 thin-helper binds, confirmed live by the
census rather than by a clock: 4 000 000 -> 1 187 and 3 200 000 -> 83 140.

Divide through and the ratios are exact, which is what makes the rest of this
section arithmetic rather than a hypothesis:

* one **wide** absolute accessor = **2** native calls (`session()` plus the
  `ScopedMemoryAccess` store);
* one **byte** absolute accessor = **1** native call (`DirectByteBuffer.put(IB)`,
  served in `native-io/src/direct_buffer.rs` since 2026-08-05).

And the measured costs, same binary, same run: direct `putLong` **290 ns**,
direct `put(byte)` **121 ns**. Two rungs against one, at ~145 ns per rung. The
accessors were never paying for WIDTH. They were paying for the extra rung.

That also retires this page's own headline number. It said `putLong` cost
1088 ns and `put(byte)` 282 ns; on current `dev` at a comparable host load they
are 290 and 121. Those numbers moved with the host and with `dev`, and the
earlier ones were taken at a load this page did not record.

### The fix, and its A/B

`java/nio/DirectByteBuffer`'s wide absolute accessors — `get/putShort`,
`get/putChar`, `get/putInt`, `get/putLong`, `get/putFloat`, `get/putDouble` —
are now served alongside the byte pair already there, out of the same fields
plus `bigEndian`. That collapses `session()` and the `ScopedMemoryAccess` store
into one native call.

Interleaved, two rounds, one binary per arm, host load 5.5-5.8:

| arm | before r1 | after r1 | before r2 | after r2 |
|---|---:|---:|---:|---:|
| direct `putInt` | 288.65 | **131.19** | 290.37 | **131.40** |
| direct `putLong` | 299.42 | **132.31** | 290.66 | **136.92** |
| direct `getLong` | 311.53 | **116.75** | 317.85 | **120.15** |
| *control* direct `put(byte)` | 121.09 | 119.46 | 122.36 | 120.88 |
| *control* heap `put(byte)` | 37.03 | 36.00 | 36.34 | 41.52 |
| *control* heap `putLong` (not served) | 393.69 | 376.46 | 391.28 | 415.50 |

**2.2-2.7x**, and the three controls do not move — including heap `putLong`,
which is the same width through the same two rungs on a class this change does
not register. A repeat in a different window reproduced it exactly: 290.30 ->
134.02 (`putLong`), 384.96 -> 116.28 (`getLong`).

The refusals are the byte accessors' refusals — unresolvable layout, index out
of range, read-only receiver, an address the memory layer declines — so the
exception this VM raises is always the JDK class-file body's own.

### It does not move this class, and the census said so first

Interleaved A/B on the class's actual work, `NettyZipBombPhases snappy 8`, G1,
two rounds: 18 586 / 18 716 ms before against 18 734 / 23 075 ms after — inside
the run-to-run spread, with the spread itself larger than any effect.

That is not a disappointment, it is a prediction confirming. The native census
of the snappy phase (below) shows the path uses `DirectByteBuffer.get(int)` —
the BYTE accessor, one native call, served since 2026-08-05 — 4 261 826 times,
and the wide accessors barely at all.

**An earlier reading of this same comparison claimed 1.45x, and it was wrong.**
Two rounds under the engine-default collector gave 14 629 / 15 740 ms before
against 10 083 / 10 043 after, which looks like a clean result and is not one:
repeating the baseline in a later window put the *unchanged* binary at
10 936 ms. The host drifted between the pairs. The accessor table above
survives that test — it reproduces at two different loads with its own controls
flat — and this one did not.

## The wall is snappy

`testZipBomb`'s five parameterisations are the encodings netty offers: `gzip`,
`deflate`, `br`, `zstd`, `snappy`. The first four are 16-19 s each. The fifth
is 566 s.

`NettyZipBombPhases` reproduces it away from JUnit, 8 MiB, G1, host load ~5:

| | CratonVM | HotSpot 25 | ratio |
|---|---:|---:|---:|
| `snappy` compress | 6 650 ms | 158 ms | **42x** |
| `snappy` decompress | 11 643 ms | 193 ms | **60x** |
| `gzip` compress (16 MiB) | 885 ms | 131 ms | 6.8x |
| `gzip` decompress (16 MiB) | 358 ms | 53 ms | 6.8x |

`gzip` is 6.8x because its kernel is this VM's zlib natives, which are near
parity. `snappy` is netty's own pure Java — so it measures compiled-Java
throughput on a byte-shuffling loop, and there it is **7-9x worse than this
VM's general ratio**. That gap is the thing to explain; the 6.8x is not.

### What snappy executes: 26.7 M native calls per 4 MiB

`--dump-native-registry` on `NettyZipBombPhases snappy 4`:

| invocations | native |
|---:|---|
| **21 368 822** | `java/lang/invoke/VarHandle.get([Ljava/lang/Object;)Ljava/lang/Object;` |
| 4 261 826 | `java/nio/DirectByteBuffer.get(I)B` |
| 523 872 | `java/lang/invoke/VarHandle.set([Ljava/lang/Object;)V` |
| 197 536 | `java/nio/DirectByteBuffer.put(IB)Ljava/nio/ByteBuffer;` |
| 132 791 | `java/lang/Enum.ordinal()I` |
| | **26 673 142 total** |

That is **~5.1 `VarHandle.get` and ~1 `DirectByteBuffer.get` per output byte**.

`VarHandle.get` is netty 4.2's reference-count check: `AbstractByteBuf`'s
checked accessors call `ensureAccessible()` -> `refCnt()`, and in 4.2 that
field is read through a `VarHandle` rather than the `AtomicIntegerFieldUpdater`
the 2026-08-17 page measured. That page saw this and parked it — "`refCnt` is a
real and large defect for every *checked* netty accessor; it is just not this
page's". It is this page's now: with `writeZero` no longer the cost, the
checked accessors are what is left.

A `perf record` of the same phase agrees and adds nothing a counter did not.
The profile is flat and its head is the funnel itself —
`try_jit_site_cached_native_dispatch` 6.9%, `forward_jit_reference_args` 3.8%,
`try_varhandle_instance_field_read` 3.2%, `safe_native_call_impl` 2.5% — plus
ZGC's `is_object_address` and `ZObjectStarts::contains` at 10% combined, which
is the same funnel's receiver validation.

There is already a fast path for this native
(`vm/src/jit/helpers.rs::try_varhandle_instance_field_read`, keyed on the
handle's identity hash, no name lookup). It sits INSIDE the generic funnel, so
it saves the field resolution and pays the ~145 ns call floor anyway. Pricing
it the way `Preconditions.checkIndex` and `Reference.reachabilityFence` were
priced — a thin `*_DIRECT_FN` bind, measured at 143 -> 23 ns for those two — is
the next measurable step. By the arithmetic above it is worth roughly a quarter
of this class, not all of it.

### The collector is not the variable

G1 against ZGC on the same snappy phase, both arms: 18 586 / 18 716 (G1)
against 17 607 / 16 520 (ZGC). The
`every-jit-getfield-takes-the-helper-because-the-guarded-inline-check-always-fails-20260817`
residual is ZGC-only and would have shown here as a G1 advantage; it does not.
`getfield helper calls: 35 287 469` in a 12 s snappy run is real and is that
page's, but it is not what separates these arms.

## The IR stack-argument blocker is CLOSED

The 2026-08-17 page ended by naming one blocker: `emit_direct_cross_call` in
the IR (optimizing) backend was register-only and required
`num_args + needs_context <= ENTRY_ABI_REGS.len()`, which is 4 on Windows, so
`ScopedMemoryAccess.putIntUnaligned` — receiver plus five arguments plus the
context pointer, seven slots — could not be bound to a thin helper at the door
that compiles it.

It marshals arguments past the register file onto the stack now, mirroring the
single-pass backend's `x64::frames::emit_stack_arg_setup` exactly: reserve the
block (Win64 shadow space included, rounded to 16 so the `CALL` stays aligned),
materialise the stack arguments through RAX first, then the register arguments,
then `CALL`, then release. Every source is `[rbp - off]`, which `SUB RSP` does
not disturb.

Two things that were true before and still are, because they are why this is
sound:

* the CALLEE side is unchanged — `lower()` still refuses a graph with more
  parameters than `incoming_abi_reg_capacity()`, so a JIT-compiled callee with
  more parameters than the register file can only have a SINGLE-PASS body, and
  that prologue reads stack-passed parameters through `emit_load_caller_arg` at
  the offsets `stack_arg_block_size` writes them to;
* a thin `extern "C"` VM helper reads its stack arguments the way the platform
  C ABI says, and always could.

Pinned by `a_direct_call_past_the_register_file_marshals_its_tail_on_the_stack`
and its narrow-path sibling in `jit/src/ir_lower.rs`, and verified by BREAKING
what they guard: restoring the old register-file gate fails the first and
leaves the second passing.

Note what this does NOT do. With the wide accessors served natively the
`ScopedMemoryAccess` rungs are off the `DirectByteBuffer` path entirely, so
nothing on that path uses the new lowering. Its live consumer is the ordinary
one — any statically-bound Java callee with more arguments than the register
file, whose `direct_calls` entry the binding side had already resolved and the
lowerer silently dropped.

## What was ruled out, with the measurement that ruled it out

* **`writeZero`, and the 1780x this page was built on.** The 2026-08-17 page
  measured `buffer.writeZero(1 MiB)` at 534 ms/MiB against HotSpot's 0.3 and
  called it "the whole cost". On current `dev` the enclosing compress phase is
  **55 ms/MiB**, and the wide-accessor fix — exactly the fix that hypothesis
  prescribed — does not move it at all. The `EmbeddedChannel` allocator's
  buffer is not reached through `ByteBuffer.putLong` on this path; the census
  names `DirectByteBuffer.get(int)` and `VarHandle.get` instead.
* **netty refusing `sun.misc.Unsafe`.** Unchanged from 2026-08-17: HotSpot 25
  reports the identical `hasUnsafe()=false` with the identical cause, so the
  non-Unsafe path is the path HotSpot also takes.
* **The compression codec, for `gzip`.** Still within 1.3x — and that is
  precisely why `snappy`, which has no native kernel, is 42-60x while `gzip` is
  6.8x.
* **The collector.** See "The collector is not the variable".

## Repro

```bash
cd apps/netty-suite-runner
printf 'io.netty.handler.codec.http.HttpContentDecompressorTest\n' > /tmp/one.txt
./run-netty-suite.sh --list /tmp/one.txt --gc g1 --shards 1 --timeout 900 --out runs/repro
```

Per-test decomposition, which is the only form that says WHICH test the budget
went to (the suite harness prints a line only for a failing test):

```bash
cratonvm --java-home <jdk> -cp <suite-cp> -Djunit.jupiter.execution.timeout.mode=disabled PerTestProgressRunner io.netty.handler.codec.http.HttpContentDecompressorTest
```

The probes that carry the numbers above:

```bash
cratonvm --java-home <jdk> -cp <suite-cp> NettyZipBombPhases snappy 8
```

```bash
cratonvm --java-home <jdk> -cp <suite-cp> NioAccessorRate 800000 20
```

```bash
cratonvm --java-home <jdk> -cp <suite-cp> --dump-native-registry=/tmp/reg.json NettyZipBombPhases snappy 4
```

```bash
cratonvm --java-home <jdk> -cp . NioAccessorOracle
```

`NioAccessorOracle` must print HotSpot's TOTAL exactly; it is the correctness
pin for every accessor this page touches.

## Related

* `httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md`,
  `httpresponsestatustest-exhaustive-loop-timeout-20260816.md` — the other two
  `codec-http` walls from the same batch. After this re-diagnosis they are the
  SAME mechanism as what is left here rather than a different one: per-call
  cost in compiled code.
* `every-jit-getfield-takes-the-helper-because-the-guarded-inline-check-always-fails-20260817.md`
  — owns the ZGC `getfield` residual this class also pays.
* `adaptive-bytebuf-allocator-throughput-20260812.md` — the same per-entry
  transfer machinery, reached from a different netty class.
