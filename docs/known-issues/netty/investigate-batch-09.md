# netty — investigate batch 09 of 13

**14 of 15 RESOLVED (2026-08-12); one class filed.** Every class was measured
against a stock HotSpot JDK 25 baseline first, which is what split the page.

**One CratonVM bug found and fixed:** `SpdyHeaderBlockZlibDecoderTest` (9/9 on
HotSpot, 5/9 here) failed every compressed header block with `Invalid Header
Block`. `Inflater.setDictionary` and `Deflater.setDictionary` were no-ops, so
zlib **preset dictionaries** — which SPDY relies on — did nothing. The stated
reason for the no-op ("flate2's `set_dictionary` is gated behind a zlib backend
feature we don't enable") was false: flate2 has been built with
`features = ["zlib"]` all along. See
[netty-inflater-preset-dictionary-no-op-FIXED-20260812.md](../../internal/fixed-suite-bugs/netty-inflater-preset-dictionary-no-op-FIXED-20260812.md).
The reach goes well past SPDY — any format using a zlib preset dictionary was
unreadable *and* unwritable.

**Six classes needed no work** — `FlowControlHandlerTest`,
`LoggingHandlerTest`, `HttpProxyHandlerTest`, `ProxyHandlerTest`,
`SpdyFrameDecoderTest` and `SpdyUnknownFrameDecoderTest` already matched
HotSpot on the dev tip (`HttpProxyHandlerTest` was repaired by batch 07's
`StackWalker$Option` fix).

**Seven `NativeImageHandlerMetadataTest`s** (redis, sctp, smtp, socks, stomp,
xml, proxy) **fail identically on HotSpot** — the same harness gap batch 08
documented: the test builds its metadata path from Maven group/artifact system
properties that Surefire sets and the fork-per-class runner does not, so the
path reads `.../native-image/null/null/...`. Not VM defects.

**One class filed, not fixed:** `PcapWriteHandlerTest` (25/25 on HotSpot,
18/25 here). It is three independent residuals, not one bug — a
`NioDatagramChannel.bind()` that throws `StacklessClosedChannelException` (with
a ~50-line repro needing no netty test), TCP close packets that are never
written although `handlerRemoved` demonstrably runs, and a >4 GB test that
times out. See
[pcap-write-handler-three-residuals-20260812.md](pcap-write-handler-three-residuals-20260812.md).

Original triage notes follow.

**No investigation done — class names and repro only.** Part of a 184-class FAIL/HANG list split across 13 pages (see [investigate-INDEX.md](investigate-INDEX.md)) so work doesn't overlap. This page owns exactly the 15 classes below — do not touch classes listed in other batch pages.

Found during the full 657-class, 3-GC-variant (default/G1/ZGC) suite run on Windows (binary built from an isolated worktree at commit `70c8b8cd6`). "status seen" reflects what each GC variant's run actually recorded — a class can be `FAIL` in one variant and `HANG` in another (shown as `FAIL/HANG` in the status column); that's raw data, not yet explained. Cross-check against stock HotSpot (`--hotspot` flag) before concluding anything is CratonVM-specific — the already-confirmed CratonVM bugs (JNI-native-codec SIGSEGV, buffer-test throughput gap) are documented separately in `docs/internal/fixed-bugs/netty-jni-native-codec-sigsegv-FIXED-20260812.md (FIXED 2026-08-12)`; the classes on these pages are NOT yet confirmed to be CratonVM defects.

## Classes

| class | status seen | GC variant(s) |
|---|---|---|
| `io.netty.handler.codec.redis.NativeImageHandlerMetadataTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.sctp.NativeImageHandlerMetadataTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.smtp.NativeImageHandlerMetadataTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.socks.NativeImageHandlerMetadataTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.spdy.SpdyFrameDecoderTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.spdy.SpdyHeaderBlockZlibDecoderTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.spdy.SpdyUnknownFrameDecoderTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.stomp.NativeImageHandlerMetadataTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.xml.NativeImageHandlerMetadataTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.flow.FlowControlHandlerTest` | FAIL | default=FAIL |
| `io.netty.handler.logging.LoggingHandlerTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.pcap.PcapWriteHandlerTest` | HANG | default=HANG, g1=HANG, zgc=HANG |
| `io.netty.handler.proxy.HttpProxyHandlerTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.proxy.NativeImageHandlerMetadataTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.proxy.ProxyHandlerTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |

## Repro

```bash
cd apps/netty-suite-runner
echo <ClassName> > /tmp/one.txt
CV_BIN=bin/cratonvm-netty-default.exe bash run-netty-suite.sh --list /tmp/one.txt --gc default --shards 1 --timeout 180 --out /tmp/repro
# swap --gc default for g1 / zgc to match the variant(s) that showed the failure
# HotSpot cross-check: bash run-netty-suite.sh --list /tmp/one.txt --hotspot --shards 1 --timeout 180 --out /tmp/repro-hs
```

