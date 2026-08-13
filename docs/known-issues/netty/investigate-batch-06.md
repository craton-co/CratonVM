# netty — investigate batch 06 of 13

**Status: TRIAGED 2026-08-12. No CratonVM defect on this page.** Nine of
fifteen classes pass outright, two fail identically on HotSpot, three are
wall-clock only, and the last is a timing-marginal flake that passes solo.

Part of a 184-class FAIL/HANG list split across 13 pages (see
[investigate-INDEX.md](investigate-INDEX.md)) so work doesn't overlap. This page
owns exactly the 15 classes below — do not touch classes listed in other batch
pages.

Originally found during the full 657-class, 3-GC-variant (default/G1/ZGC) suite
run on Windows (binary built from an isolated worktree at commit `70c8b8cd6`).
"status seen" is what that run recorded; "explained by" is the 2026-08-12
triage on the Azure Linux host (`20.80.105.49`), binary built from `origin/dev`
`747a7c433`.

## Outcome

**The `http2` cluster is green.** All seven `io.netty.handler.codec.http2`
classes on this page were recorded FAIL and now pass — 74/74, 51/51, 50/50,
42/42, 23/23, 7/7, 2/2 — which is the batch-07
[`StackWalker$Option` fix](../../internal/fixed-suite-bugs/netty-stackwalker-option-clinit-nameless-constants-FIXED-20260813.md)
landing, exactly as that page predicted (the broken `Enum.valueOf` sat on the
interception path of every Mockito mock). Same for all three `websocketx`
classes.

The remaining five split three ways, none of them a CratonVM defect:

* **Two fail on HotSpot too** — `haproxy` and `http`
  `NativeImageHandlerMetadataTest`, the same GraalVM native-image metadata
  fixture as batch 04's three and batch 05's one.
* **Three are wall-clock only**, and one of them is worth reading in full
  (below).
* **One is timing-marginal**: `DefaultHttp2ConnectionTest` measured 46/50 then
  **50/50** on a solo re-run.

## `HttpResponseStatusTest` is a 2.1-billion-iteration loop

Worth recording because it looks alarming and is not:

```java
// status scope: [Integer.MIN_VALUE, 100).
for (int code = Integer.MIN_VALUE; code < 100; code++) {
    HttpStatusClass httpStatusClass = HttpStatusClass.valueOf(code);
    assertEquals(HttpStatusClass.UNKNOWN, httpStatusClass);
}
```

That is ~2.1 × 10⁹ iterations, each with a `valueOf` and a JUnit
`assertEquals`. HotSpot compiles it to almost nothing; CratonVM pays its
per-call cost 2.1 billion times. A `--nojit --stack-dump-on-timeout` run caught
the main thread exactly there
(`testHttpStatusClassValueOf → Assertions.assertEquals → AssertionUtils
.objectsAreEqual → Enum.equals`). No deadlock, no defect — the single most
extreme call-density case in the suite.

`HttpHeaderValidationUtilTest` is the same shape at a smaller scale: 5 506
tests. `HttpContentDecompressorTest` is the compression family of
[batch 05](investigate-batch-05.md).

## Classes

Legend: ✅ matches HotSpot · ⚪ fails on HotSpot too · ⏱ wall-clock only ·
⚠ timing-marginal

| class | status seen | explained by |
|---|---|---|
| `io.netty.handler.codec.haproxy.NativeImageHandlerMetadataTest` | FAIL | ⚪ **fails identically on HotSpot** |
| `io.netty.handler.codec.http.HttpContentDecompressorTest` | HANG | ⏱ decompression codecs → [batch 05](investigate-batch-05.md) |
| `io.netty.handler.codec.http.HttpHeaderValidationUtilTest` | HANG | ⏱ 5 506 tests (HotSpot 5 506/5 506) |
| `io.netty.handler.codec.http.HttpResponseStatusTest` | HANG | ⏱ **~2.1 × 10⁹ loop iterations** in `testHttpStatusClassValueOf`, see above |
| `io.netty.handler.codec.http.NativeImageHandlerMetadataTest` | FAIL | ⚪ **fails identically on HotSpot** |
| `io.netty.handler.codec.http.websocketx.WebSocket08FrameDecoderTest` | FAIL | ✅ **3/3** |
| `…websocketx.extensions.WebSocketClientExtensionHandlerTest` | FAIL | ✅ **4/4** |
| `…websocketx.extensions.WebSocketServerExtensionHandlerTest` | FAIL | ✅ **6/6** |
| `io.netty.handler.codec.http2.CleartextHttp2ServerUpgradeHandlerTest` | FAIL | ✅ **7/7** |
| `io.netty.handler.codec.http2.DataCompressionHttp2Test` | FAIL | ✅ **42/42** |
| `io.netty.handler.codec.http2.DecoratingHttp2ConnectionEncoderTest` | FAIL | ✅ **2/2** |
| `io.netty.handler.codec.http2.DefaultHttp2ConnectionDecoderTest` | FAIL | ✅ **74/74** |
| `io.netty.handler.codec.http2.DefaultHttp2ConnectionEncoderTest` | FAIL | ✅ **51/51** |
| `io.netty.handler.codec.http2.DefaultHttp2ConnectionTest` | FAIL | ⚠ **50/50 solo**; 46/50 under 3-way sharding. `testRemoveAllStreams` does `assertTrue(latch.await(5, SECONDS))` and the class takes 140–180 s on CratonVM vs 1.8 s on HotSpot, so that 5 s bound is marginal here. Not a correctness defect; it will keep flaking until the per-call gap closes |
| `io.netty.handler.codec.http2.DefaultHttp2FrameReaderTest` | FAIL | ✅ **23/23** |

## Repro

Run these solo (`--shards 1`): `DefaultHttp2ConnectionTest` is decided by a 5 s
in-test deadline, so shard contention alone changes its verdict.

```bash
cd apps/netty-suite-runner
printf 'io.netty.handler.codec.http2.DefaultHttp2ConnectionTest\n' > /tmp/one.txt
bash run-netty-suite.sh --list /tmp/one.txt --shards 1 --timeout 600 \
  --bin <cratonvm> --out /tmp/repro
```
