# netty's `AdaptivePoolingAllocator` runs ~196x HotSpot — where the time goes

**Characterised 2026-08-17. Not fixed.** This page exists so the next attempt
starts from measurements. It is the one live failure left in
`io.netty.handler.pcap.PcapWriteHandlerTest`, and the reason that class reads as
a HANG in the netty suite.

`AdaptiveByteBufAllocator` is netty 4.2's DEFAULT `ByteBufAllocator`, so this is
not a corner: every netty application on this VM allocates through it.

## The workload

`PcapWriteHandlerTest.writePcapGreaterThan4Gb` pushes ~8.6 GB through an
`EmbeddedChannel` carrying `PcapWriteHandler`: ~131 000 iterations of
`writeInbound(payload.retainedDuplicate())` with a 65 495-byte chunk, each
building Ethernet/IP/TCP headers and copying the payload out to a counting
`OutputStream`.

| | wall |
|---|---:|
| HotSpot 25 | **3.8 s** |
| CratonVM G1 | **294 s** (304 s on a second binary) |

Reduced to a standalone loop (same handler chain, same chunk, warmed):

| | per iteration | throughput |
|---|---:|---:|
| HotSpot | 13.2 us | 4727 MB/s |
| CratonVM | 2588 us | 24 MB/s |

**196x.** For comparison, CratonVM's ordinary interpreted-vs-JIT gap on this
codebase runs 2–20x, and its bulk memory primitives are 3–4x (see below), so
this row is an outlier that wants explaining rather than absorbing.

## It is not the copies

Bulk memory movement is fine. Measured directly:

| op | HotSpot | CratonVM | ratio |
|---|---:|---:|---:|
| `Unsafe.copyMemory` 64 KiB | 0.03 ns/byte | 0.10 ns/byte | 3.3x |
| `direct.put(direct)` 64 KiB | 1414 ns | 6287 ns | 4.4x |
| `direct.put(byte[])` 64 KiB | 1301 ns | 4612 ns | 3.5x |
| `direct.get(byte[])` 64 KiB | 1232 ns | 4643 ns | 3.8x |

At four 65 KiB copies per iteration that is ~25 us of the 2588 us — about 1%.

(`Unsafe.setMemory` WAS the outlier here at 20.6 ns/byte, 600x HotSpot, and with
it `ByteBuffer.allocateDirect(64K)` at 7.1 ms. Both are fixed on
`fix/netty-nio-pcap-tls-residuals-20260817` — setMemory now goes through the same
bulk bridge `copyMemory` uses, 126x faster, and `allocateDirect` is within 1.9x
of HotSpot. It moved this workload by nothing measurable, because the allocator
recycles its chunks: allocateDirect runs a few hundred times, not 131 000.)

## It is the allocator, interpreted at its root

Leaf profile, `--stack-sample-ms 20`, 3367 samples on the main thread:

| | share |
|---|---:|
| `AdaptivePoolingAllocator$Magazine.allocate` | **16.4%** |
| `java/nio/ByteBuffer.put` | 13.5% |
| `java/nio/DirectByteBuffer.<init>` | 12.3% |
| `PcapWriter.writePacket` | 6.7% |
| `PcapWriteHandler.completeTCPWrite` | 6.5% |
| `PcapWriteHandler.handleTcpPacket` | 4.9% |
| `AdaptivePoolingAllocator$AdaptiveByteBuf.init` | 4.4% |
| `ByteBuffer.<init>` / `HeapByteBuffer.<init>` / `MappedByteBuffer.<init>` | 6.0% |
| `BuddyChunk.readInitInto` / `SizeClassedChunk.readInitInto` | 3.7% |
| `SizeClassedChunk.nextAvailableSegmentOffset` / `BuddyChunk.chooseFirstFreeBuddy` | 2.9% |

Roughly 40% of the run is inside the adaptive allocator and its `ByteBuffer`
construction, and `Magazine.allocate` — the root of that subtree — **never
compiles**. `CRATONVM_DBG=jit-method-stats`:

```
still-interpreted=6 c1=33 c2=226 | hot_but_stuck_in_interpreter=1 (compile-failures=1)
  26865 queued=false tier_fail_count=3  compile-failed
    io/netty/buffer/AdaptivePoolingAllocator$Magazine.allocate(IILio/netty/buffer/AdaptivePoolingAllocator$AdaptiveByteBuf;Z)Z
    reason=rbc6-handler-reads-unsafe-local(pc=338,op=0xbb)
```

226 other methods in this run do reach C2, so the JIT is working; this one method
is refused.

## The blocker, and why it is not a one-line admission

Opcode `0xbb` is `new`. RBC.6 admits a method whose exception handlers read
non-parameter locals only when every potentially-throwing bytecode inside a
protected range exits through a call site that publishes a precise (reason-9/10)
exceptional frame — `precise_frame_publishing_opcode` in `jit/src/lib.rs`. The
invokes, monitors, `getfield`/`putfield`, `getstatic` and `checkcast` are all
admitted; allocation, array ops, divide, `athrow` and `ldc` are not, because
their lowerings do not publish that snapshot yet.

Admitting `new` without giving its lowering a precise frame is a **miscompile**,
not a speedup: every local beyond the parameters resumes as 0/null in the handler
and the method returns a wrong answer silently. Compare
`fixed-suite-bugs/rbc6-protected-field-ops-FIXED-20260802.md`, where the
admission was legitimate precisely because the codegen had already grown the
publishing call site.

And RBC.6 bails on the FIRST unadmitted opcode, so clearing `new` at pc=338 may
only move the bail: list every unadmitted opcode inside this method's protected
ranges before pricing the work.

## The ceiling, measured

Do not price this as "compile one more method". The JIT is worth 1.8x on this
loop in total:

| arm | per iteration |
|---|---:|
| CratonVM, JIT on | 2280 us |
| CratonVM, `--nojit` | 4083 us |
| HotSpot | 13.2 us |

So compiled code here is under 2x better than interpreting, and unblocking
`Magazine.allocate` cannot bridge 196x on its own. Whatever is spent per
allocation is spent in roughly the same way compiled or not — which points at the
per-call machinery (allocation, `ByteBuffer` construction, native dispatch) rather
than at code quality inside one body. `new Object()` measures 674 ns interpreted
against HotSpot's 10 ns, and the pcap loop runs at about that ratio, which is the
number to explain.

## Repro

```bash
cd apps/netty-suite-runner
# the microbenchmark (probes-nettyres/PcapThroughput.java in the fixture)
<cv-bin> --java-home "$JDK" --Xmx 1500m -XX:+UseG1GC -Diters=2000 \
    -cp "probes-nettyres;$(sed -n 2p common.args)" PcapThroughput
CRATONVM_DBG=jit-method-stats <cv-bin> ... PcapThroughput      # names the stuck method
<cv-bin> --nojit ... PcapThroughput                            # the 1.8x ceiling
# the real test
printf 'io.netty.handler.pcap.PcapWriteHandlerTest\n' > /tmp/one.txt
./run-netty-suite.sh --list /tmp/one.txt --gc g1 --shards 1 --timeout 600 --out runs/repro
```

## Related

* `fixed-suite-bugs/netty/pcapwritehandlertest-is-not-a-reopening-FIXED-20260817.md`
  — the suite row this blocks, and the `Unsafe.setMemory` fix the profiling found.
* `perf-bintrees-9x-gap-characterised.md` — the other characterised-not-fixed
  allocation-throughput page; same shape of question, different workload.
