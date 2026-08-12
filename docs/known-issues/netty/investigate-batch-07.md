# netty — investigate batch 07 of 13

**RESOLVED (2026-08-12).** All 15 classes on this page had **one** root cause:
`java.lang.StackWalker$Option`'s native `<clinit>` fabricated enum constants
with a null `Enum.name()`, so `Enum.valueOf` could never match one — which
broke `org.mockito.internal.debugging.Java9PlusLocationImpl.<clinit>`, and with
it every Mockito-backed test in netty's http2 module. Full analysis in
[stackwalker-option-clinit-nameless-constants-20260812.md](stackwalker-option-clinit-nameless-constants-20260812.md).
All 15 classes now pass on the Linux host, matching stock HotSpot
test-for-test (343 tests, 0 failures).

Two residuals worth knowing about, neither a correctness defect:

* `HpackEncoderTest` (~50 s) and `Http2ConnectionRoundtripTest` (~39 s) run
  5×/3× slower than HotSpot (9.5 s / 13.9 s). They pass on Linux at the 180 s
  cap; the `HANG` recorded below for `HpackEncoderTest` on Windows is that
  throughput gap meeting the flat wall cap under 6-way sharding, not a
  deadlock. Covered by the existing interpreter/JIT throughput work, not filed
  again here.
* `Http2FrameCodecTest` reports `aborted=2` on **both** VMs — JUnit assumption
  failures in the fixture, identical on HotSpot. The harness scores
  `aborted>0` as `ABORTED` rather than `PASS`, so this class will not show as
  `PASS` in a suite run even though it matches HotSpot exactly.

Original triage notes follow.

**No investigation done — class names and repro only.** Part of a 184-class FAIL/HANG list split across 13 pages (see [investigate-INDEX.md](investigate-INDEX.md)) so work doesn't overlap. This page owns exactly the 15 classes below — do not touch classes listed in other batch pages.

Found during the full 657-class, 3-GC-variant (default/G1/ZGC) suite run on Windows (binary built from an isolated worktree at commit `70c8b8cd6`). "status seen" reflects what each GC variant's run actually recorded — a class can be `FAIL` in one variant and `HANG` in another (shown as `FAIL/HANG` in the status column); that's raw data, not yet explained. Cross-check against stock HotSpot (`--hotspot` flag) before concluding anything is CratonVM-specific — the already-confirmed CratonVM bugs (JNI-native-codec SIGSEGV, buffer-test throughput gap) are documented separately in `docs/internal/fixed-bugs/netty-jni-native-codec-sigsegv-FIXED-20260812.md (FIXED 2026-08-12)`; the classes on these pages are NOT yet confirmed to be CratonVM defects.

## Classes

| class | status seen | GC variant(s) |
|---|---|---|
| `io.netty.handler.codec.http2.DefaultHttp2FrameWriterTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.http2.DefaultHttp2LocalFlowControllerTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.http2.HpackDecoderTest` | FAIL/HANG | default=FAIL, g1=HANG, zgc=FAIL |
| `io.netty.handler.codec.http2.HpackEncoderTest` | HANG | default=HANG, g1=HANG, zgc=HANG |
| `io.netty.handler.codec.http2.Http2ConnectionHandlerTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.http2.Http2ConnectionRoundtripTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.http2.Http2ControlFrameLimitEncoderTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.http2.Http2EmptyDataFrameConnectionDecoderTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.http2.Http2EmptyDataFrameListenerTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.http2.Http2FrameCodecTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.http2.Http2FrameRoundtripTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.http2.Http2MaxRstFrameConnectionDecoderTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.http2.Http2MaxRstFrameLimitEncoderTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.http2.Http2MaxRstFrameListenerTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.codec.http2.Http2MultiplexCodecTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |

## Repro

```bash
cd apps/netty-suite-runner
echo <ClassName> > /tmp/one.txt
CV_BIN=bin/cratonvm-netty-default.exe bash run-netty-suite.sh --list /tmp/one.txt --gc default --shards 1 --timeout 180 --out /tmp/repro
# swap --gc default for g1 / zgc to match the variant(s) that showed the failure
# HotSpot cross-check: bash run-netty-suite.sh --list /tmp/one.txt --hotspot --shards 1 --timeout 180 --out /tmp/repro-hs
```

