# netty — investigate batch 04 of 13

**No investigation done — class names and repro only.** Part of a 184-class FAIL/HANG list split across 13 pages (see [investigate-INDEX.md](investigate-INDEX.md)) so work doesn't overlap. This page owns exactly the 15 classes below — do not touch classes listed in other batch pages.

Found during the full 657-class, 3-GC-variant (default/G1/ZGC) suite run on Windows (binary built from an isolated worktree at commit `70c8b8cd6`). "status seen" reflects what each GC variant's run actually recorded — a class can be `FAIL` in one variant and `HANG` in another (shown as `FAIL/HANG` in the status column); that's raw data, not yet explained. Cross-check against stock HotSpot (`--hotspot` flag) before concluding anything is CratonVM-specific — the already-confirmed CratonVM bugs (JNI-native-codec SIGSEGV, buffer-test throughput gap) are documented separately in `docs/known-issues/netty/jni-native-codec-sigsegv-20260812.md`; the classes on these pages are NOT yet confirmed to be CratonVM defects.

## Classes

| class | status seen | GC variant(s) |
|---|---|---|
| `io.netty.channel.NativeImageHandlerMetadataTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.channel.group.DefaultChannelGroupTest` | FAIL | g1=FAIL |
| `io.netty.channel.nio.NioEventLoopTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.channel.nio.NioIoHandlerEventCountTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.channel.oio.OioEventLoopTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.channel.pool.FixedChannelPoolMapDeadlockTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.channel.socket.nio.NioDatagramChannelTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.channel.socket.nio.NioServerDomainSocketChannelTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.channel.socket.nio.NioSocketChannelTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.channel.unix.NativeInetAddressTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.NativeImageHandlerMetadataTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.AdaptiveCumulatorTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.MessageAggregatorTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.NativeImageHandlerMetadataTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.compression.BrotliIntegrationTest` | HANG | default=HANG, g1=HANG, zgc=HANG |

## Repro

```bash
cd apps/netty-suite-runner
echo <ClassName> > /tmp/one.txt
CV_BIN=bin/cratonvm-netty-default.exe bash run-netty-suite.sh --list /tmp/one.txt --gc default --shards 1 --timeout 180 --out /tmp/repro
# swap --gc default for g1 / zgc to match the variant(s) that showed the failure
# HotSpot cross-check: bash run-netty-suite.sh --list /tmp/one.txt --hotspot --shards 1 --timeout 180 --out /tmp/repro-hs
```

