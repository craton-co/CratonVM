# `ZipContentTests` runs 10-14x HotSpot — it is `java.nio.ByteBuffer` scalar accessors, not GC, not disk

**Status: OPEN — root-caused and priced. Supersedes the GC-pressure framing in
`zipcontenttests-gc-pressure-timeout-not-disk-capacity-20260807.md`, whose two
hypotheses are both refuted below by measurement. Filed 2026-08-10.**

## It is not a hang

Run with no 300s ceiling, on Windows, current dev + the 2026-08-09 SSL-cluster
fixes:

| arm | result |
|---|---|
| HotSpot 25, `-Xmx 2g` | **PASS 29/29 in 22.4s** |
| CratonVM, `-Xmx 2g`, JIT | **PASS 29/29 in 315.5s** (14.1x) |
| CratonVM, `-Xmx 8g`, JIT | **PASS 29/29 in 308.9s** (13.8x) |
| CratonVM, `-Xmx 2g`, `--nojit` | **PASS 29/29 in 219.1s** (9.8x) |

Every arm passes. Zero failures, zero aborts. The 300s suite budget is the only
thing this class ever hit, and it clears it by 5%.

## Both prior hypotheses are refuted

**"GC/allocation pressure from the multi-gigabyte fixture."** `-Xmx 8g` is
308.9s against `-Xmx 2g`'s 315.5s — a 2% difference, inside run-to-run noise.
Quadrupling the heap changes nothing, so heap pressure is not the driver. (For
contrast, the same experiment on
`HttpComponentsClientHttpConnectorBuilderTests` turned a 300s overrun into a
24.3s pass, so the lever is real and this class simply does not respond to it.)
The 2g run logs 9 `[moving-young] fallback` lines and one `gc::guard` retention
over 315s — present, but not a churn story.

**"The stall is before or during test discovery, because `.out.log` is 0
bytes."** `SbRunner` prints nothing until `launcher.execute(req)` **returns**
(`sb-runner/SbRunner.java:80,83`). A 0-byte `.out.log` on a class that produces
no logging of its own means "did not finish" and nothing more; it cannot locate
the stall. This class writes no log output, so the file is 0 bytes right up to
the moment it would print the whole summary.

**"The disk-capacity gotcha."** Already ruled out by the prior page and still
true here: 178GB free, and the fixture's own
`assumeTrue(getFreeSpace() > 6GB)` passes — all 29 tests run.

## The heap lever under `-XX:+UseZGC` — added 2026-08-10

The refutation above deliberately speaks only for the default collector, and
flagged that the ZGC arm's `OutOfMemoryError` (recorded in
`internal/fixed-suite-bugs/springboot/zgc-real-fullsuite-regression-RETIRED-20260808.md`
§3) still had no heap-size A/B behind it. It has one now, same host, same
protocol — one class per process, no 300s ceiling:

| arm | result |
|---|---|
| CratonVM default, `-Xmx 2g`, JIT | **PASS 29/29 in 301s** |
| CratonVM `-XX:+UseZGC`, `-Xmx 2g` | **OOM at 262s** (`native primitive array of length 8192`) |
| CratonVM `-XX:+UseZGC`, `-Xmx 3g` | **PASS 29/29 in 314s** |
| CratonVM `-XX:+UseZGC`, `-Xmx 4g` | **PASS 29/29 in 312s** |
| CratonVM `-XX:+UseZGC`, `-Xmx 8g` | **PASS 29/29 in 263s** |

So the lever this class does **not** respond to under the default collector is
the difference between an OOM and a pass under ZGC, and the requirement is
modest and flat: between 2g and 3g, unchanged from 3g to 8g. That is what a
non-compacting whole-arena sweep costs on this allocation pattern, not a
fragmentation cliff. It changes nothing about this page's own finding — the
`ByteBuffer` accessor cost is collector-independent, and is what makes every
one of those arms 10-14x HotSpot.

## Where the time goes

`--stack-sample-ms=200` over the `--nojit` arm (1052 leaf samples) gives a flat
profile — no frame above 8%:

```
 7.7%  java/nio/HeapByteBuffer.getShort      2.4%  java/nio/HeapByteBuffer.byteOffset
 4.8%  java/util/zip/InflaterInputStream.read 2.1%  java/util/Objects.checkFromIndexSize
 4.8%  java/nio/Buffer.nextGetIndex          2.0%  java/util/zip/ZipOutputStream.writeLOC
 4.7%  java/nio/HeapByteBuffer.getInt        1.7%  org/springframework/…/ZipCentralDirectoryFileHeaderRecord.load
```

Grouped: **`ByteBuffer` scalar reads ≈ 18%**, zip deflate/inflate/framing ≈ 14%,
AssertJ file diffing ≈ 9%. A flat profile is why this looked like "everything is
just slower".

It is not everything. `probes/ZipContentTermsProbe.java` prices each term the
sampler named, same process, both VMs, ns/op:

| term | HotSpot | CratonVM | ratio |
|---|---:|---:|---:|
| `ByteBuffer.getShort`+`getInt` | 1.36 | **2234.14** | **1643x** |
| `Deflater.deflate` 32MB | 4.14 | 5.78 | 1.4x |
| `Inflater.inflate` 32MB | 0.73 | 1.49 | 2.0x |
| `CRC32.update` 32MB | 0.11 | 0.83 | 7.5x |
| `FileOutputStream.write` 256MB | 0.66 | 0.47 | **0.7x** |
| `FileInputStream.read` 256MB | 0.53 | 0.33 | **0.6x** |
| `ZipOutputStream` 20k entries | 273 µs | 362 µs | 1.3x |

File I/O is **faster** than HotSpot here, and zlib is within 2x. One term is off
by three orders of magnitude, and it is the one Spring Boot's zip reader calls
once per header field.

## The gap has two layers

`probes/ByteBufferScalarSplitProbe.java` separates them. `HeapByteBuffer.get(int)`
is pure Java (`hb[ix(checkIndex(i))]`, no native); `getShort`/`getInt` go on
through `SCOPED_MEMORY_ACCESS.get*Unaligned`, which is a registered native here.

| arm | HotSpot | CratonVM | ratio |
|---|---:|---:|---:|
| raw `byte[]` element read | 0.43 | 3.69 | 8.6x |
| `buffer.get(int)` | 1.54 | **242.24** | **157x** |
| `buffer.getShort(int)` | 1.44 | **1161.77** | **807x** |
| `buffer.getInt(int)` | 1.60 | **1334.90** | **834x** |
| hand-rolled LE short (2 × `get`) | 2.61 | 711.69 | 273x |

Reading down that table:

1. **Array access itself is fine** — 3.69 ns, ordinary interpreter/JIT overhead.
2. **Layer 1, ~240 ns:** a *compiled* `HeapByteBuffer.get(int)` called from a
   *compiled* loop costs 242 ns. There is no native on that path — it is
   `get` → `checkIndex` → `Buffer.checkIndex` → `ix`, four compiled calls. That
   is the per-call dispatch floor, ~60 ns a link against the ~8.4 ns an ordinary
   Java call is documented at. The hand-rolled arm confirms it scales with call
   count: 711 ns ≈ 2 × 242 plus the arithmetic.
3. **Layer 2, ~900 ns on top:** `getShort`/`getInt` add the
   `ScopedMemoryAccess` native crossing.

**Not a compile refusal.** With `CRATONVM_DBG_JIT_COMPILED=1` every method on
the path is compiled — `HeapByteBuffer.get(I)B`, `getShort(I)S`, `getInt(I)I`,
`checkIndex(I)I`, `checkIndex(II)I`, `ix(I)I`, `byteOffset(J)J` — and all five
probe loops are OSR-compiled. The cost is in the calls that *do* happen, not in
falling back to the interpreter.

This is also why `--nojit` is **faster than JIT** for the class as a whole
(219.1s vs 315.5s): the JIT is not what makes these accessors slow, and it
carries costs of its own here.

## What to fix — and the attempt that says where

The accessors were intrinsified as registered natives on
`java/nio/HeapByteBuffer` and measured end to end. The implementation is in
`native-builtins/src/phases_late/nio_buffer.rs`, **default OFF**, enabled with
`CRATONVM_BYTEBUFFER_INTRINSIC=1`. It is off by default because of this:

| arm | intrinsic ON | intrinsic OFF | |
|---|---:|---:|---|
| `--nojit` | **187.0s** | 206.5s | intrinsic **9.4% faster** |
| JIT, round 1 | 464.3s | 413.7s | intrinsic 12.2% slower |
| JIT, round 2 | 375.8s | 353.8s | intrinsic 6.2% slower |
| JIT, earlier pair | 477.3s | 450.7s | intrinsic 5.9% slower |

The accessors themselves get faster exactly as predicted —
`getShort()` 1.42x, `getInt()` 1.5x, `getShort(int)` 2.09x, `getInt(int)`
2.14x — and the class still gets slower under the JIT, in three independent
interleaved pairs.

### The mechanism is NOT established — an earlier claim here was wrong

This page first said the loss was the JIT no longer being able to INLINE these
accessors once a native was registered. **That explanation does not hold.** The
single-pass emitter "bails on any callee invoke that is not a resolver-proven
elidable super-`<init>`" (`jit/src/lib.rs:5239`), and
`HeapByteBuffer.getShort()`'s body is four invokes — `scope()`, `checkIndex`,
`byteOffset`, and the `ScopedMemoryAccess` call. It was never inlined into its
callers, so there was no inlining to lose. The JIT does COMPILE all of those
methods (`CRATONVM_DBG_JIT_COMPILED=1` lists them), which is a different thing.

The class-level numbers also deserve less weight than they were given. Across
one session the SAME configuration — intrinsic OFF, JIT — measured 315.5s,
353.8s, 413.7s and 450.7s on this host. That is a ±20% spread, and the three
ON-vs-OFF deltas (5.9%, 6.2%, 12.2%) sit inside it. The pairs were run back to
back, which controls for slow drift, but each pair is a single run: the
direction is consistent, the magnitude is not established.

What IS solid is the microbenchmark — small, repeated, interleaved, min-of-3,
with the deliberately-excluded arms measuring 1.00x and 0.97x, which shows the
lever is scoped to exactly the accessors it claims.

So the open question is why a strictly shorter path did not show a class-level
win, and the honest answer is that nobody has measured it yet. Candidates worth
separating: run-to-run variance swamping a real small gain; inline-cache /
dispatch differences at a native call site versus a compiled Java one; or the
accessors simply not being a large enough share of this class's time for a 2x
on them to move the total (they were ~18% of leaf samples, so the ceiling is
about 9%). The last one is arithmetic and can be checked without a build.

The arithmetic is worth doing before any more runs, because it bounds the whole
question. The accessors were ~18% of leaf samples. Making them 2.4x faster
removes `18% x (1 - 1/2.4)` = **~10%** of total time, and that is the CEILING.
The measured deltas are 6-12% against a configuration whose own spread is ±20%
— so this experiment could never have separated a 10% win from a 10% loss. It
was underpowered by construction, and reading a mechanism out of it was the
mistake.

The `--nojit` arm is the one that lands where the arithmetic predicts: **9.4%
faster** against a ~10% ceiling. That agreement is the strongest evidence on
this page that the intrinsic does what it claims, and it is why the JIT arm's
sign should not be trusted without a properly powered measurement (repeated
pairs, or per-process CPU time rather than wall clock).

Whatever the reason, the conclusion for this page is unchanged:

1. **The intrinsic belongs in the JIT**, as a compiled inlinable intrinsic
   rather than a registered native. The contract is already pinned by
   `probes/ByteBufferAccessorMatrixProbe.java` (119 lines, identical to HotSpot
   in both arms) and the byte assembly is written and unit-tested; what has to
   change is the layer, not the logic.
2. **The per-call dispatch floor** is the general problem underneath both
   numbers and reaches far past NIO —
   `perf/per-call-dispatch-floor-20260803` is the existing work. A compiled
   `HeapByteBuffer.get(int)` with no native anywhere on its path still costs
   ~300 ns against HotSpot's 1.54.

Two smaller findings worth keeping:

* `get(int)` and `get()` are deliberately NOT intrinsified. They are the only
  accessors here whose Java body makes no native call, so a crossing makes them
  *slower* (0.76x measured). Intrinsifying everything that looked alike would
  have cost throughput on the commonest accessor of the set.
* The first version covered only the ABSOLUTE forms and moved this class not at
  all — its header reader calls `getShort()`/`getInt()` and the absolute forms
  never. Reach before speed: a shape the hot code never executes cannot show a
  win however fast it is.

## Reproducers

- `probes/ZipContentTermsProbe.java` — prices every term the sampler named.
- `probes/ByteBufferScalarSplitProbe.java` — splits accessor cost by layer, and
  covers both the absolute and relative forms.
- `probes/ByteBufferAccessorMatrixProbe.java` — the HotSpot-diffed contract:
  every buffer shape, both byte orders, bounds against `limit`, exception class
  and message per width, and NaN/`-0.0` bit patterns. 119 lines, and the diff
  against HotSpot is the test.

All three print plain text and run in under a minute on either VM.

## Affected classes

`loader/spring-boot-loader` — `org.springframework.boot.loader.zip.ZipContentTests`
is where this was caught, and it is not special: any class that parses binary
headers through `ByteBuffer` pays the same per-field cost.
