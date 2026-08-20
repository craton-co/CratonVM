# `PcapWriteHandlerTest` — not a reopening. The real-socket family is intact; one throughput test eats the budget

**Status: CLOSED 2026-08-17** as a reopening claim, branch
`fix/netty-nio-pcap-tls-residuals-20260817`, Windows host, `cratonvm.exe`
release build, G1. Supersedes the OPEN page
`pcapwritehandlertest-hang-reopened-20260816`, which read a silent 180 s kill as
"most likely a reopening of the same real-socket family the 2026-08-13 fix
closed". It is not. Every real-socket UDP and TCP test in the class passes.

## What the class actually does, with the harness cap taken off

`--timeout` raised, per-test wall time recorded (one process, JUnit's own
concurrent execution, so the numbers overlap):

| | found | ok | failed |
|---|---|---|---|
| HotSpot 25 | 25 | 25 | 0 |
| CratonVM G1, dev + this branch | 25 | **24** | **1** |

The single failure is `writePcapGreaterThan4Gb`, at **294 s** (dev) and 304 s
(this branch) against HotSpot's **3.8 s**. Everything the page suspected is
green:

| test | HotSpot | CratonVM |
|---|---:|---:|
| `udpV4SharedOutputStreamTest` | 4211 ms | 2048 ms |
| `udpV4NonOutputStream` | 4211 ms | 2054 ms |
| `udpV4NoGlobalHeaderOutputStream` | 4211 ms | 2184 ms |
| `tcpV4SharedOutputStreamTest` | 4306 ms | 2176 ms |
| `tcpV4NoGlobalHeaderOutputStream` | 4306 ms | 2181 ms |
| `tcpV4NonOutputStream` | 4304 ms | 2627 ms |
| `writePcapGreaterThan4Gb` | **3787 ms** | **294 459 ms** |

So the 2026-08-13 fixes hold: §1c (`FdTable::udp_rebind` +
`selector_refresh_udp`, the UDP `bind()`-breaks-fd-identity defect) and §2 (the
deleted zero-quiet-period `shutdownGracefully` registrations) are both still in
the tree and still working — see
`fixed-suite-bugs/netty-pcap-write-handler-udp-bind-and-tcp-close-FIXED-20260813.md`.
Nothing needed re-fixing.

## Why it read as a HANG

Two caps, both smaller than 294 s:

* the harness's flat 180 s per-class process cap — `rc=124`, reported HANG;
* `-Djunit.jupiter.execution.timeout.default=120s` from `common.args` — so even
  with the process cap raised the test is reported FAILED, not slow.

And the page's inference from the log was reasonable but wrong. It saw only two
lines of output, from `udpLargeByteBufPayload` and `udpLargeDatagramPayload`, and
concluded those were the only tests that had run. They are simply the only two
tests in the class that **log anything** (netty's own `{}`-placeholder warning).
A stack dump at 90 s settles it: the main thread is `blocked=false` — RUNNING,
not parked — inside

```
PcapWriteHandlerTest.writePcapGreaterThan4Gb
  -> PcapWriteHandler.channelRead -> handleTCP -> handleTcpPacket -> completeTCPWrite
```

A "silent hang" that is actually a running thread is worth checking with a dump
before it is filed as a hang: `blocked=false` in the T19.H1 summary is the whole
tell.

## What the investigation DID find: `Unsafe.setMemory` was 20 ns per BYTE

Profiling the 4 GiB test led to a real, general defect, fixed on this branch.
`Unsafe.setMemory`'s off-heap arm was a byte-at-a-time loop over
`unsafe_arena_put_byte`, and each of those takes the arena store's `RwLock` for
writing and re-runs a `BTreeMap` range probe to locate the block:

| op | HotSpot | CratonVM before | CratonVM after |
|---|---:|---:|---:|
| `Unsafe.setMemory` 64 KiB | 2.1 us | **1 352 550 ns** | **10 697 ns** |
| `Unsafe.copyMemory` 64 KiB | 2.0 us | 6 797 ns | 8 507 ns |
| `ByteBuffer.allocateDirect(64K)` | 29 144 ns | **7 077 399 ns** | **54 788 ns** |
| `ByteBuffer.allocateDirect(256K)` | 95 367 ns | 22 784 457 ns | 133 870 ns |

`copyMemory` had always been bulk (it goes through `ctx.copy_to_native_memory`,
which resolves the block once), so the VM was **200x slower than itself** on the
same range. `DirectByteBuffer.<init>` zeroes its whole allocation with
`UNSAFE.setMemory(base, size, (byte) 0)`, which made this a tax on every
`ByteBuffer.allocateDirect(n)`, linear in n — 126x on the primitive, 129x on the
allocation, and `allocateDirect` is now within 1.4–1.9x of HotSpot instead of
243x. It is a win for every direct-buffer workload in the VM.

It did **not** move this test (294 s → 304 s, i.e. noise), because netty's
`AdaptivePoolingAllocator` recycles its chunks: the allocation happens a few
hundred times, not 131 000. That is worth stating plainly — the fix is real and
large and is not this test's cost.

## The residual, and its ceiling

The 4 GiB test's cost is netty's buffer-allocator machinery executed ~131 000
times, and it is **not** reachable by one more compile:

* `--nojit` vs JIT on the same hot loop: 4083 us/op vs 2280 us/op. The JIT is
  worth **1.8x** here, so compiled-code quality is the gap, not a blocked method.
* `CRATONVM_DBG=jit-method-stats` names exactly one hot-but-stuck method,
  `AdaptivePoolingAllocator$Magazine.allocate` (26 865 invocations),
  `reason=rbc6-handler-reads-unsafe-local(pc=338,op=0xbb)`. Opcode 0xbb is `new`,
  which `precise_frame_publishing_opcode` deliberately excludes — admitting it
  needs the allocation lowering to publish a precise reason-9/10 frame first, and
  admitting a non-publishing opcode is a miscompile, not a speedup. 226 other
  methods DO reach C2 in this run.
* leaf profile (3367 samples): `Magazine.allocate` 16.4%, `ByteBuffer.put` 13.5%,
  `DirectByteBuffer.<init>` 12.3%, the pcap handler itself ~18%, the rest spread
  across `AdaptiveByteBuf.init`, `readInitInto`, `chooseFirstFreeBuddy`,
  `nextAvailableSegmentOffset`.

Characterised separately, with the microbenchmark, and **closed 2026-08-17** —
see `performance/netty-adaptive-allocator-throughput-FIXED-20260817`. The
refusal above is gone: `new` and `athrow` both publish a precise exceptional
frame now and both are admitted, so `Magazine.allocate` compiles and
`hot_but_stuck_in_interpreter` reads 0. Note what that page found about the
0xbb refusal reported here — the `athrow` seven bytes later was unadmitted too,
so clearing 0xbb alone would only have moved the bail. This class remains a
throughput row, which is where the 2026-08-13 page had already filed it (its
§4, "correct, slow, and re-filed where it belongs").

## Repro

```bash
cd apps/netty-suite-runner
printf 'io.netty.handler.pcap.PcapWriteHandlerTest\n' > /tmp/one.txt
# the harness cap reports HANG; raise it and the per-test JUnit default to see 24/25
./run-netty-suite.sh --list /tmp/one.txt --gc g1 --shards 1 --timeout 600 --out runs/repro
```
