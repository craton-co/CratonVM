# netty — investigate batch 12 of 13

**No investigation done — class names and repro only.** Part of a 184-class FAIL/HANG list split across 13 pages (see [investigate-INDEX.md](investigate-INDEX.md)) so work doesn't overlap. This page owns exactly the 15 classes below — do not touch classes listed in other batch pages.

Found during the full 657-class, 3-GC-variant (default/G1/ZGC) suite run on Windows (binary built from an isolated worktree at commit `70c8b8cd6`). "status seen" reflects what each GC variant's run actually recorded — a class can be `FAIL` in one variant and `HANG` in another (shown as `FAIL/HANG` in the status column); that's raw data, not yet explained. Cross-check against stock HotSpot (`--hotspot` flag) before concluding anything is CratonVM-specific — the already-confirmed CratonVM bugs (JNI-native-codec SIGSEGV, buffer-test throughput gap) are documented separately in `docs/known-issues/netty/jni-native-codec-sigsegv-20260812.md`; the classes on these pages are NOT yet confirmed to be CratonVM defects.

## Classes

| class | status seen | GC variant(s) |
|---|---|---|
| `io.netty.util.NettyRuntimeTests` | FAIL/HANG | default=HANG, g1=FAIL, zgc=FAIL |
| `io.netty.util.RecyclerFastThreadLocalTest` | FAIL | default=FAIL, zgc=FAIL |
| `io.netty.util.RecyclerTest` | FAIL | default=FAIL, zgc=FAIL |
| `io.netty.util.ResourceLeakDetectorTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.util.ThreadDeathWatcherTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.util.concurrent.AutoScalingEventExecutorChooserFactoryTest` | FAIL | default=FAIL, g1=FAIL |
| `io.netty.util.concurrent.DefaultPromiseTest` | HANG | default=HANG, g1=HANG, zgc=HANG |
| `io.netty.util.concurrent.DefaultThreadFactoryTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.util.concurrent.FastThreadLocalTest` | HANG | default=HANG, g1=HANG, zgc=HANG |
| `io.netty.util.concurrent.NonStickyEventExecutorGroupTest` | FAIL/HANG | default=FAIL, g1=HANG, zgc=FAIL |
| `io.netty.util.concurrent.PromiseAggregatorTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.util.concurrent.PromiseCombinerTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.util.concurrent.PromiseNotifierTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.util.internal.JfrEventSafeTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.util.internal.TypeParameterMatcherTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |

## Repro

```bash
cd apps/netty-suite-runner
echo <ClassName> > /tmp/one.txt
CV_BIN=bin/cratonvm-netty-default.exe bash run-netty-suite.sh --list /tmp/one.txt --gc default --shards 1 --timeout 180 --out /tmp/repro
# swap --gc default for g1 / zgc to match the variant(s) that showed the failure
# HotSpot cross-check: bash run-netty-suite.sh --list /tmp/one.txt --hotspot --shards 1 --timeout 180 --out /tmp/repro-hs
```

