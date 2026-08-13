# netty — investigate batch 11 of 13

**TRIAGED 2026-08-12, filed not fixed.** All 15 classes measured against a stock
HotSpot JDK 25 baseline; full record in
[netty-batch11-inet6-and-sha1-oid-CLOSED-20260812.md](../../internal/fixed-suite-bugs/netty-batch11-inet6-and-sha1-oid-CLOSED-20260812.md) — **closed 2026-08-12**.
Eleven classes have real gaps spread across **seven independent causes**, so
this is filed rather than fixed in one pass — but two of them are isolated to a
three-line repro.

**Subtract the environment before counting.** Four classes are not CratonVM
defects (three pass on both; `NativeImageHandlerMetadataTest` fails on both).
Two more are badly overstated by raw totals — **24 of `CertificateBuilderTest`'s
failures are `SLH-DSA KeyPairGenerator not available` on HotSpot too**, and
**15 of `SslHandlerTest`'s are `UnsatisfiedLinkError` for netty-tcnative on
both**. Counting raw would have inflated CratonVM's delta by 39 tests.
`DnsNameResolverTest` also hits the cap on HotSpot.

The seven causes, best entry points first:

1. **The DNS transport never comes up** — `DnsAddressResolverGroupTest`
   ("couldn't setup transport" + `ClosedChannelException`), `SearchDomainTest`
   (hangs; `NoSuchMethodError: DatagramChannel.localAddress()` kills the
   resolver thread) and `DnsNameResolverTest`. The signature matches the
   **already-filed** [`NioDatagramChannel.bind()` throwing
   `StacklessClosedChannelException`](pcap-write-handler-three-residuals-20260812.md)
   from batch 09. Try that fix first — it may clear three classes here and
   three there.
2. **`Inet6Address.getByAddress` folds an IPv4-mapped address to 4 bytes**
   (`NetUtilTest`). HotSpot keeps 16; CratonVM returns an `Inet6Address`
   carrying an IPv4 address, so netty indexes past the end. Three-line repro in
   the record; `InetAddress.getByAddress` and `getByName` are both correct, so
   the fold is being applied one factory too far.
3. **`sha1WithRSAEncryption` (1.2.840.113549.1.1.5) is unimplemented** in the
   chain verifier (`SslContextTrustManagerTest`, all 4). Only SHA256-RSA and
   ECDSA-SHA256 are implemented, though `oid_to_sig_name` already recognises
   the wider family and the digests exist.
4. **ML-DSA / ML-KEM KeyPairGenerators missing** (12) plus a missing
   `X509CertImpl.getAlgorithm()` (3) — the latter is the cheapest item here.
5. **The TLS handshake/alert family** (`SslHandlerTest` ~10, OCSP 3) — same
   shape as [batch 10's clusters 3 and 4](tls-batch10-encrypted-keys-and-handshake-gaps-20260812.md);
   investigate together, not twice.
6. **BouncyCastle provider identity** (`BouncyCastleUtilTest`, both tests).
7. **`HashedWheelTimerTest`** — one wall-clock bound missed by 8 ms; the
   throughput gap, not a correctness defect.

Original triage notes follow.

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

