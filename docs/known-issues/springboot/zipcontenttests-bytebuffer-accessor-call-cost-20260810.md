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

## What to fix

Two independent levers, in order of leverage for this class:

1. **Intrinsify the `HeapByteBuffer` absolute scalar accessors**
   (`getShort/getInt/getLong/getChar(int)` and the `put` mirrors) as a single
   native that reads the backing `hb` array at `offset + index` in the buffer's
   byte order. That collapses both layers at once for the exact shape a zip or
   protocol header reader uses. Watch the contract: bounds are checked against
   `limit` (not `capacity`), the exception is `IndexOutOfBoundsException`, and
   `HeapByteBufferR` shares the read path — verify per shape against a HotSpot
   control rather than assuming one rule covers them.
2. **The per-call dispatch floor** (layer 1) is the general problem and reaches
   far past NIO; `perf/per-call-dispatch-floor-20260803` is the existing work.

Fixing only (2) leaves ~900 ns of layer 2. Fixing only (1) still leaves every
other NIO caller paying layer 1.

## Reproducers

- `probes/ZipContentTermsProbe.java` — prices every term the sampler named.
- `probes/ByteBufferScalarSplitProbe.java` — splits accessor cost by layer.

Both print CSV and run in under a minute on either VM.

## Affected classes

`loader/spring-boot-loader` — `org.springframework.boot.loader.zip.ZipContentTests`
is where this was caught, and it is not special: any class that parses binary
headers through `ByteBuffer` pays the same per-field cost.
