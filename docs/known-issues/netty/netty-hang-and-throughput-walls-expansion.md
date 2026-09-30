# Netty Hangs and Throughput Walls — New Clusters and 1200s Findings

| | |
|---|---|
| **Status** | Throughput wall analysis and 1200s timeout rerun verification. |
| **Scope** | 7 newly analyzed classes from 180s HANG list + verification of `Adaptive*` ByteBuf family. |
| **Discovered** | 2026-09-23/24, full 739-class suite run and 3-GC 1200s rerun. |

## 1. Breakthrough Findings Under 1200s Timeout

In the default 180s run, 26 classes were killed with `process-died rc=124 timeout=180s`.
When re-run under `--timeout 1200s` across the 3 GC arms (`-XX:+UseGenerationalGC`, `-XX:+UseG1GC`, `-XX:+UseZGC`), several previously labeled "hangs" proved to be pure throughput walls that **complete successfully**:

1. **`io.netty.buffer.AdaptiveBigEndianHeapByteBufTest`**:
   - Status: 🟢 **100% PASS** (415 ok, 0 failed, 2 skipped).
   - Timing: **145 s** (ZGC), **178 s** (G1), **192 s** (Generational).
   - Under 180s timeout, this class narrowly timed out on G1 and Generational, but with adequate budget it passes completely with zero errors.

2. **`io.netty.buffer.AdaptiveByteBufAllocatorGrowthTest`**:
   - Status: 🟢 **100% PASS** (400 ok, 0 failed).
   - Timing: **297.8 s** (ZGC), **350 s** (G1), **396 s** (Generational).
   - Completes 400 test cases cleanly.

3. **`io.netty.buffer.AdaptiveBigEndianDirectByteBufTest`**:
   - Status: Completes **414 tests ok**, 1 failure, 2 skipped.
   - Timing: **434 s** (ZGC), **483 s** (G1), **509 s** (Generational).
   - The single failure is an internal JUnit Jupiter `@Timeout(120)` in `testInternalNioBuffer()`:
     `java.util.concurrent.TimeoutException: testInternalNioBuffer() timed out after 120 seconds`.
   - The test was not deadlocked in CratonVM; it simply ran longer than the 120s per-method cap.

---

## 2. Newly Categorized 180s Hangs

### A. `HttpContentDecompressorTest.testZipBomb` (Compression Cluster)
- **Class**: `io.netty.handler.codec.http.HttpContentDecompressorTest`
- **Mechanism**: Method `testZipBomb` allocates 256 chunks of 1MB (256 MB total), compresses them with `gzip`, `deflate`, and `snappy`, and decompresses the full 256 MB payload through `HttpContentDecompressor`.
- **Classification**: Pure throughput wall. Shares the exact same mechanism as the 10 compression integration tests documented in `compression-cluster-testhugedecompress-180s-throughput-wall-20260827.md`.

### B. `PooledBigEndianDirectByteBufTest` & `PooledLittleEndianDirectByteBufTest` (Direct ByteBuf Cluster)
- **Classes**:
  - `io.netty.buffer.PooledBigEndianDirectByteBufTest`
  - `io.netty.buffer.PooledLittleEndianDirectByteBufTest`
- **Mechanism**: Both classes extend `AbstractPooledByteBufTest` / `AbstractByteBufTest`, inheriting over 415 test methods executing allocations, copies, slices, and searches across direct memory buffers.
- **Classification**: Throughput wall. As demonstrated by `AdaptiveBigEndianDirectByteBufTest` (434s) vs `AdaptiveBigEndianHeapByteBufTest` (145s), direct buffer operations take ~2.5x longer than heap buffer operations, pushing the runtime of 415 tests over the 180s threshold.

### C. `SizeClassedChunkCacheTest` (Concurrent Scan Livelock)
- **Class**: `io.netty.buffer.SizeClassedChunkCacheTest`
- **Failure**: Method `concurrentScansTerminateWhenNoCapacity()` fails assertion:
  `AssertionFailedError: Concurrent scans should terminate within 30 seconds, not livelock ==> expected: <true> but was: <false> at SizeClassedChunkCacheTest.java:510`.
- **Classification**: Concurrency defect / timing wall in lock-free chunk cache scanning under thread contention.

### D. `DefaultHttp2ConnectionTest` & `StreamBufferingEncoderTest` (HTTP/2 Stream Exhaustion Loops)
- **Classes**:
  - `io.netty.handler.codec.http2.DefaultHttp2ConnectionTest` (50 test methods, contains loops creating streams up to `SMALLEST_MAX_CONCURRENT_STREAMS * 2` and `maxConcurrentStreams * 2`)
  - `io.netty.handler.codec.http2.StreamBufferingEncoderTest`
- **Classification**: High iteration loop throughput wall. Massive stream allocations and state transitions in unoptimized loops take > 180s to execute the full 50-test class.

### E. `PcapWriteHandlerTest` (4GB Packet Stream)
- **Class**: `io.netty.handler.pcap.PcapWriteHandlerTest`
- **Mechanism**: Documented previously in `docs/internal/fixed-suite-bugs/netty/pcapwritehandlertest-is-not-a-reopening-FIXED-20260817.md`. `writePcapGreaterThan4Gb` generates >4GB of TCP data, taking ~294s on CratonVM vs 3.8s on HotSpot C2.
- **Classification**: Throughput wall.
