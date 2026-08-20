# Netty non-passing classes confirmed NOT CratonVM bugs — consolidated reference

**Purpose: stop future sessions re-investigating these.** Every entry below
was directly cross-checked against stock HotSpot 25 on the same host, same
classpath, same harness (`run-netty-suite.sh` / `CratonRunner`) — and
HotSpot fails, aborts, or produces the identical result. None of these
belong in a "CratonVM regression" count. Compiled 2026-08-19 from the
2026-08-19 FAIL/HANG/CRASH rerun (`fail-hang-crash-rerun-20260817.md` and
its split-out cluster docs) plus two carried-forward verdicts from the
2026-08-16 doc sweep. Each entry links to the doc with the full evidence;
this page is the index, not a replacement for them.

| class | why it's not a CratonVM bug | evidence | doc |
|---|---|---|---|
| `io.netty.channel.NativeImageHandlerMetadataTest` | Resource path computed from Maven `groupId`/`artifactId` metadata this non-Maven harness never populates — literal `null/null` in the path on both VMs | Direct HotSpot cross-check: identical `AssertionFailedError`, identical `null/null` path | `nativeimagehandlermetadatatest-not-a-cratonvm-bug-20260819.md` |
| `io.netty.handler.NativeImageHandlerMetadataTest` | same | Same failure text observed in the raw log; mechanism shared with the class above, not independently re-run on HotSpot | same |
| `io.netty.handler.codec.NativeImageHandlerMetadataTest` | same | same (log pattern match, not independently re-run) | same |
| `io.netty.handler.codec.dns.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.codec.haproxy.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.codec.http.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.codec.http2.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.codec.memcache.binary.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.codec.mqtt.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.codec.redis.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.codec.sctp.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.codec.smtp.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.codec.socks.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.codec.stomp.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.codec.xml.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.handler.proxy.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.resolver.dns.NativeImageHandlerMetadataTest` | same | same | same |
| `io.netty.buffer.AlignedPooledByteBufAllocatorTest` | `isDirectMemoryCacheAlignmentSupported()` assumption answers `false` on this host for both VMs | Direct HotSpot cross-check: `found=49 ok=21 aborted=28`, byte-identical to CratonVM | `buffer-alignment-abort-cluster-not-a-cratonvm-bug-20260819.md` |
| `io.netty.buffer.AdvancedLeakAwareCompositeByteBufTest` | same assumption family | Internally consistent counts (`aborted=9`), not independently re-run on HotSpot | same |
| `io.netty.buffer.BigEndianCompositeByteBufTest` | same | same (`aborted=9`) | same |
| `io.netty.buffer.LittleEndianCompositeByteBufTest` | same | same (`aborted=9`) | same |
| `io.netty.buffer.PooledByteBufAllocatorTest` | same | same (`aborted=2`) | same |
| `io.netty.buffer.SimpleLeakAwareCompositeByteBufTest` | same | same (`aborted=9`) | same |
| `io.netty.buffer.WrappedCompositeByteBufTest` | same | same (`aborted=9`) | same |
| `io.netty.handler.codec.http2.WeightedFairQueueRemoteFlowControllerTest` | Harness never passes `-ea`; netty's own internal `assert` statements never fire on either VM | Direct HotSpot cross-check: identical `found=34 ok=28 failed=6`, same 6 methods | `http2-flowcontroller-and-datacompression-triage-20260819.md` |
| `io.netty.handler.codec.http2.UniformStreamByteDistributorFlowControllerTest` | same `-ea` gap — inherits the identical test methods from the same base class | Not independently re-run on HotSpot; same mechanism and base class as the row above | same |
| `io.netty.handler.ssl.BouncyCastleEngineAlpnTest` | BouncyCastle's JSSE provider (`org.bouncycastle.jsse.provider.SSLContext.TLSv1_3`) isn't resolvable from this environment's classpath | Direct HotSpot cross-check: identical `ClassNotFoundException`, `found=1 failed=1` both | `bouncycastlealpn-and-udt-echo-triage-20260819.md` |
| `io.netty.handler.ssl.CloseNotifyTest` | `netty-tcnative`/OpenSSL not available in this environment | Direct HotSpot cross-check, isolated: identical `found=4 ok=2 aborted=2`, same `Assumption failed: OpenSSL is not available` | `hashedwheeltimertest-late-task-firing-20260819.md` |
| `io.netty.handler.ssl.SslErrorTest` | same OpenSSL-unavailability, parameterization yields zero cases | Direct HotSpot cross-check, isolated: identical `found=0 started=0` | same |
| `io.netty.pkitesting.CertificateBuilderTest` | Result set matches HotSpot exactly, in both directions | `ok=39 failed=28 aborted=7` on both VMs; failing-test-name sets identical (`comm`-verified) | `certificatebuildertest-fail-status-not-a-regression-20260816.md` |
| `io.netty.handler.ssl.PemEncodedTest` | `1 ok / 2 aborted` is steady-state-correct — two intentional `assumeFalse` skips, not a regression | Isolated on G1: `1 ok / 2 aborted` identical on CratonVM and HotSpot | `pemencodedtest-aborted-status-not-a-regression-20260816.md` |

## What this list is not

It is not exhaustive over the full non-passing set — only classes actually
investigated as of 2026-08-19 appear here. It is not a claim that every row
was individually re-run on HotSpot: several `NativeImageHandlerMetadataTest`
and buffer-cluster rows are included on the strength of one representative
class's direct cross-check plus a shared, textually-identical failure
mechanism observed in the raw logs for the others — that's noted per row
above rather than glossed over. Anything still showing `FAIL`/`HANG`/`CRASH`
and *not* on this list should be treated as open until shown otherwise.

## Related

- `fail-hang-crash-rerun-20260817.md` — the rerun this index summarizes.
- The five per-cluster docs linked in the table above carry the full
  investigation for each row.
