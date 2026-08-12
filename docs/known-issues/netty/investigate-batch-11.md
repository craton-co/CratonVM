# netty — investigate batch 11 of 13

**No investigation done — class names and repro only.** Part of a 184-class FAIL/HANG list split across 13 pages (see [investigate-INDEX.md](investigate-INDEX.md)) so work doesn't overlap. This page owns exactly the 15 classes below — do not touch classes listed in other batch pages.

Found during the full 657-class, 3-GC-variant (default/G1/ZGC) suite run on Windows (binary built from an isolated worktree at commit `70c8b8cd6`). "status seen" reflects what each GC variant's run actually recorded — a class can be `FAIL` in one variant and `HANG` in another (shown as `FAIL/HANG` in the status column); that's raw data, not yet explained. Cross-check against stock HotSpot (`--hotspot` flag) before concluding anything is CratonVM-specific — the already-confirmed CratonVM bugs (JNI-native-codec SIGSEGV, buffer-test throughput gap) are documented separately in `docs/internal/fixed-bugs/netty-jni-native-codec-sigsegv-FIXED-20260812.md (FIXED 2026-08-12)`; the classes on these pages are NOT yet confirmed to be CratonVM defects.

## Classes

| class | status seen | GC variant(s) |
|---|---|---|
| `io.netty.handler.ssl.SslContextTrustManagerTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.ssl.SslHandlerTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.ssl.ocsp.OcspClientTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.ssl.ocsp.OcspServerCertificateValidatorTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.handler.ssl.util.BouncyCastleUtilTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.pkitesting.CertificateBuilderTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.resolver.DefaultHostsFileEntriesResolverTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.resolver.InetSocketAddressResolverTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.resolver.dns.DnsAddressResolverGroupTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.resolver.dns.DnsNameResolverTest` | HANG | default=HANG, g1=HANG, zgc=HANG |
| `io.netty.resolver.dns.NativeImageHandlerMetadataTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.resolver.dns.SearchDomainTest` | HANG | default=HANG, g1=HANG, zgc=HANG |
| `io.netty.util.AbstractReferenceCountedTest` | FAIL | g1=FAIL |
| `io.netty.util.HashedWheelTimerTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |
| `io.netty.util.NetUtilTest` | FAIL | default=FAIL, g1=FAIL, zgc=FAIL |

## Repro

```bash
cd apps/netty-suite-runner
echo <ClassName> > /tmp/one.txt
CV_BIN=bin/cratonvm-netty-default.exe bash run-netty-suite.sh --list /tmp/one.txt --gc default --shards 1 --timeout 180 --out /tmp/repro
# swap --gc default for g1 / zgc to match the variant(s) that showed the failure
# HotSpot cross-check: bash run-netty-suite.sh --list /tmp/one.txt --hotspot --shards 1 --timeout 180 --out /tmp/repro-hs
```

