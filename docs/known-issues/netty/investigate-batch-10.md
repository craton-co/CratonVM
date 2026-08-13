# netty — investigate batch 10 of 13

**TRIAGED 2026-08-12, filed not fixed.** All 15 classes measured against a stock
HotSpot JDK 25 baseline; full record in
[netty-tls-batch10-provider-routing-and-close-notify-FIXED-20260813.md](../../internal/fixed-suite-bugs/netty-tls-batch10-provider-routing-and-close-notify-FIXED-20260813.md) (FIXED 2026-08-13); its residuals were then fixed too — R1-R5, R7, R8 — and recorded in [netty-tls-batch10-residuals-FIXED-20260813.md](../../internal/fixed-suite-bugs/netty-tls-batch10-residuals-FIXED-20260813.md) (FIXED 2026-08-13). Every class in this batch now matches or beats the HotSpot 25 oracle except `JdkSslEngineTest`, whose remaining engine-level gaps are in [jdksslenginetest-engine-level-gaps-20260813.md](jdksslenginetest-engine-level-gaps-20260813.md).
This page is the largest so far — nine classes have real CratonVM gaps and they
are **four independent causes**, not one, which is why it is filed rather than
fixed in a single pass.

**Six classes are not CratonVM defects.** `EnhancedX509ExtendedTrustManagerTest`,
`OptionalSslHandlerTest` and `PkiTestingTlsTest` pass on both;
`BouncyCastleEngineAlpnTest`, `OpenSslKeyMaterialManagerTest` and
`PemEncodedTest` fail identically on HotSpot. Two more counts are inflated by
the environment: **9 of `SslContextBuilderTest`'s 12 failures are
`UnsatisfiedLinkError` for netty-tcnative, which HotSpot hits too** (real delta
3), and `SniClientTest`'s HotSpot failure is `address already in use`. Subtract
the environment before counting.

The four real causes:

1. **`PBEWithMD5AndDES` is not implemented** (~12 failures, `SniHandlerTest` ×7
   plus the two `Jdk*ContextTest`s and `SslContextBuilderTest`). CratonVM has
   PBES2 but not PBES1, so every `test_encrypted.pem` is rejected. Note the
   in-tree discipline: admitting the algorithm name without implementing it is
   worse than refusing it, and `jca/cipher.rs` records having made exactly that
   mistake before.
2. **PKCS#1 AES-encrypted keys fail DER parsing** (~8 failures) — the *real
   JDK's* `DerValue` throws `Invalid lenByte` on bytes HotSpot parses fine, so
   the fault is upstream of the parser, in whatever produces them.
3. **Handshake completes but carries no data** (~5 failures) — AssertJ
   `expected 0 to be >= 7`.
4. **`ParameterizedSslHandlerTest` HANGS** — 6 s on HotSpot; with JUnit
   timeouts disabled CratonVM ran 17 minutes. A stack dump names it:
   `testAlertProducedAndSend` blocked forever in
   `DefaultPromise.awaitUninterruptibly()` — the TLS alert never arrives.
   Probably the same root as cause 3. `JdkSslEngineTest` also hits the cap but
   takes 178 s on HotSpot, so it has **not** been separated into hang-vs-slow.

Original triage notes follow.

**No investigation done — class names and repro only.** Part of a 184-class FAIL/HANG list split across 13 pages (see [investigate-INDEX.md](investigate-INDEX.md)) so work doesn't overlap. This page owns exactly the 15 classes below — do not touch classes listed in other batch pages.

Found during the full 657-class, 3-GC-variant (default/G1/ZGC) suite run on Windows (binary built from an isolated worktree at commit `70c8b8cd6`). "status seen" reflects what each GC variant's run actually recorded — a class can be `FAIL` in one variant and `HANG` in another (shown as `FAIL/HANG` in the status column); that's raw data, not yet explained. Cross-check against stock HotSpot (`--hotspot` flag) before concluding anything is CratonVM-specific — the already-confirmed CratonVM bugs (JNI-native-codec SIGSEGV, buffer-test throughput gap) are documented separately in `docs/internal/fixed-bugs/netty-jni-native-codec-sigsegv-FIXED-20260812.md (FIXED 2026-08-12)`; the classes on these pages are NOT yet confirmed to be CratonVM defects.

## Classes

| class | status seen | GC variant(s) |
|---|---|---|
| `io.netty.handler.ssl.ApplicationProtocolNegotiationHandlerTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.ssl.BouncyCastleEngineAlpnTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.ssl.CloseNotifyTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.ssl.EnhancedX509ExtendedTrustManagerTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.ssl.JdkSslClientContextTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.ssl.JdkSslEngineTest` | HANG | default=HANG, g1=HANG, zgc=HANG |
| `io.netty.handler.ssl.JdkSslServerContextTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.ssl.OpenSslKeyMaterialManagerTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.ssl.OptionalSslHandlerTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.ssl.ParameterizedSslHandlerTest` | HANG | default=HANG, g1=HANG, zgc=HANG |
| `io.netty.handler.ssl.PemEncodedTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.ssl.PkiTestingTlsTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.ssl.SniClientTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.ssl.SniHandlerTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.ssl.SslContextBuilderTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |

## Repro

```bash
cd apps/netty-suite-runner
echo <ClassName> > /tmp/one.txt
CV_BIN=bin/cratonvm-netty-default.exe bash run-netty-suite.sh --list /tmp/one.txt --gc default --shards 1 --timeout 180 --out /tmp/repro
# swap --gc default for g1 / zgc to match the variant(s) that showed the failure
# HotSpot cross-check: bash run-netty-suite.sh --list /tmp/one.txt --hotspot --shards 1 --timeout 180 --out /tmp/repro-hs
```

