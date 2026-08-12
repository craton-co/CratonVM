# netty — FAIL/HANG classes needing investigation (index)

**184 unique classes** (union of FAIL/HANG across all 3 GC-variant runs, 2026-08-12), split into 13 pages of up to 15 classes each so multiple people can pick up a page without duplicating work. **No investigation done here** — this is a raw class list plus a repro command template; each page's classes are its own to investigate, not shared with any other page.

Found during the full 657-class, 3-GC-variant (default/G1/ZGC) suite run on Windows (binary built from an isolated worktree at commit `70c8b8cd6`). "status seen" reflects what each GC variant's run actually recorded — a class can be `FAIL` in one variant and `HANG` in another (shown as `FAIL/HANG` in the status column); that's raw data, not yet explained. Cross-check against stock HotSpot (`--hotspot` flag) before concluding anything is CratonVM-specific — the already-confirmed CratonVM bugs (JNI-native-codec SIGSEGV, buffer-test throughput gap) are documented separately in `docs/internal/fixed-bugs/netty-jni-native-codec-sigsegv-FIXED-20260812.md (FIXED 2026-08-12)`; the classes on these pages are NOT yet confirmed to be CratonVM defects.

## Pages

- [batch 01](investigate-batch-01.md) — 15 classes (io.netty.bootstrap.BootstrapTest .. io.netty.buffer.BigEndianHeapByteBufTest)
- [batch 02](investigate-batch-02.md) — 15 classes (io.netty.buffer.BigEndianUnsafeDirectByteBufTest .. io.netty.buffer.ReadOnlyByteBufferBufTest)
- [batch 03](investigate-batch-03.md) — 15 classes (io.netty.buffer.ReadOnlyDirectByteBufferBufTest .. io.netty.channel.ManualIoEventLoopTest)
- [batch 04](investigate-batch-04.md) — 15 classes (io.netty.channel.NativeImageHandlerMetadataTest .. io.netty.handler.codec.compression.BrotliIntegrationTest)
- [batch 05](investigate-batch-05.md) — 15 classes (io.netty.handler.codec.compression.Bzip2IntegrationTest .. io.netty.handler.codec.dns.NativeImageHandlerMetadataTest)
- [batch 06](investigate-batch-06.md) — 15 classes (io.netty.handler.codec.haproxy.NativeImageHandlerMetadataTest .. io.netty.handler.codec.http2.DefaultHttp2FrameReaderTest)
- [batch 07](investigate-batch-07.md) — **RESOLVED 2026-08-12**, all 15 to one cause: [`StackWalker$Option` nameless enum constants](stackwalker-option-clinit-nameless-constants-20260812.md) (the same defect batches 12/13 hit from the other end of the suite). **Re-run the remaining pages against a build carrying that fix before investigating them.** The broken `Enum.valueOf` sat on the interception path of *every* Mockito mock, not only http2's: a 62-class control slice also repaired `StreamBufferingEncoderTest` (batch 08), `HttpProxyHandlerTest` (batch 09), `ReadOnlyByteBufTest` (batch 02) and `PromiseCombinerTest` outright, and partly repaired `LittleEndianCompositeByteBufTest` (batch 01). The FAIL counts below are inflated by an unknown amount.
- [batch 08](investigate-batch-08.md) — 15 classes (io.netty.handler.codec.http2.Http2MultiplexHandlerTest .. io.netty.handler.codec.mqtt.NativeImageHandlerMetadataTest)
- [batch 09](investigate-batch-09.md) — 15 classes (io.netty.handler.codec.redis.NativeImageHandlerMetadataTest .. io.netty.handler.proxy.ProxyHandlerTest)
- [batch 10](investigate-batch-10.md) — 15 classes (io.netty.handler.ssl.ApplicationProtocolNegotiationHandlerTest .. io.netty.handler.ssl.SslContextBuilderTest)
- [batch 11](investigate-batch-11.md) — 15 classes (io.netty.handler.ssl.SslContextTrustManagerTest .. io.netty.util.NetUtilTest)
- [batch 12](investigate-batch-12.md) — 15 classes (io.netty.util.NettyRuntimeTests .. io.netty.util.internal.TypeParameterMatcherTest)
- [batch 13](investigate-batch-13.md) — 4 classes (io.netty.util.internal.logging.CommonsLoggerTest .. io.netty.util.internal.logging.Slf4JLoggerTest)
