# FAIL/HANG/CRASH rerun — 101 classes, isolated (1 shard), all 3 collectors

**Status: results, not a closed investigation.** Rerun 2026-08-19 (Azure
host, dev base `b4d79475c`, binaries `cratonvm-netty-fhc20260817-{default,g1,zgc}`
in worktree `/data/cvm-netty-rerun-fhc-20260817`, branch
`test/netty-rerun-fhc-20260817`). Input: the union of every class that was
`FAIL`, `HANG`, or `CRASH` in *any* of the three collectors in the last full
657-class run (`C:\craton\cratonvm\apps\netty-suite-runner\runs\
{generational,g1,zgc}\run-20260816-110130-passed`, commit `3ef3eb744`) — 101
classes. Reran each, isolated (`--shards 1`, no other collector or shard
competing), against each of default/G1/ZGC.

## Headline: substantial improvement since Aug 16

| GC | fixed (old non-PASS → PASS) | still open | apparent new fails |
|---|---:|---:|---:|
| default | 26 | 57 | 3 |
| G1 | 35 | 60 | 0 |
| ZGC | 22 | 56 | 3 |

26-35 of the 101 previously-broken classes now pass cleanly per collector —
three days of `dev` landing fixes, plus this rerun's isolation (no
contention from the other collectors/shards the original run shared the
host with).

## 57 classes still non-passing on all three collectors, identically

Same distribution on every collector: **9 ABORTED, 29 FAIL, 18 HANG, 1
NOTESTS**. Full list in `/tmp/stillopen-all3.txt` on the Azure host (not
reproduced here in full — most of it is already tracked). Breaking down
what's *not* already covered by an existing doc:

**Already documented elsewhere — no new work needed here:**
`AdaptiveByteBufAllocator{Growth,UseCacheForNonEventLoopThreads,}Test`,
`SearchProcessorTest` → `adaptivebytebufallocator-searchprocessor-180s-wall-20260816.md`.
The 11-class compression `*IntegrationTest` cluster → `compression-testhugedecompress-shared-timeout-20260816.md`.
`HttpContentDecompressorTest`, `HttpHeaderValidationUtilTest`,
`HttpResponseStatusTest`, `PcapWriteHandlerTest` →
`httpcontentdecompressortest-hang-20260816.md` /
`httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md` /
`httpresponsestatustest-exhaustive-loop-timeout-20260816.md` /
`pcapwritehandlertest-hang-reopened-20260816.md`.
`SslHandlerTest`, `JdkSslEngineTest` → `openssl-key-material-and-engine-residuals-20260813.md`.
`FastThreadLocalTest` → `fastthreadlocal-2e9-iteration-throughput-wall-20260812.md`.
`ResourceLeakDetectorTest` → `resourceleakdetector-concurrentusage-timeout-20260815.md` /
`zgc-rewrite-pass-walks-off-a-reference-array-20260815.md`.
`DnsNameResolverTest`, `CertificateBuilderTest` →
`certificatebuildertest-fail-status-not-a-regression-20260816.md` and, for the
`HANG` → `ABORTED` change flagged here, **answered 2026-08-19**:
`fixed-suite-bugs/netty/dnsnameresolvertest-windows-only-aborts-CONFIRMED-20260819.md`.
Not the `0.0.0.1` bug (fixed and retired by `62cd387c1`) and not a second
issue — the 16 aborts are eight Windows-only tests × two channel strategies,
byte-identical on stock HotSpot. The `HANG` was the class running to 82% of
the 180s cap; it now has a 600s per-class override and a
`known-benign-aborts.tsv` entry so `categorize` stops re-flagging it.
`BootstrapTest`/`ServerBootstrapTest` (`UnsupportedOperationException:
CONNECT_TIMEOUT_MILLIS`) were already known from the very first triage this
session did.

**New clusters, not yet triaged:**

1. ~~**17 `NativeImageHandlerMetadataTest` classes, one per module** — all
   `FAIL→FAIL` identically on all three collectors.~~ **RESOLVED 2026-08-19,
   17 FAIL → 17 PASS:**
   `fixed-suite-bugs/netty/nativeimagehandlermetadatatest-harness-module-scope-FIXED-20260819.md`.
   One shared root cause as suspected, and not a VM one: the suite ran these
   build-hygiene tests from the fixture directory against the flat
   whole-reactor classpath, so they looked for a `null/null` resource path
   and over-collected handlers from sibling modules. Stock HotSpot failed
   them byte-identically. They now run module-scoped and pass on both VMs.
2. **7 buffer classes, `ABORTED` uniformly**:
   `AdvancedLeakAwareCompositeByteBufTest`, `AlignedPooledByteBufAllocatorTest`,
   `BigEndianCompositeByteBufTest`, `LittleEndianCompositeByteBufTest`,
   `PooledByteBufAllocatorTest`, `SimpleLeakAwareCompositeByteBufTest`,
   `WrappedCompositeByteBufTest`. Not examined — worth checking whether
   these share an `assumeTrue`/`@EnabledIf` gate — see
   `buffer-alignment-abort-cluster-not-a-cratonvm-bug-20260819.md`.
3. **3 HTTP/2 classes**: `DataCompressionHttp2Test`,
   `UniformStreamByteDistributorFlowControllerTest`,
   `WeightedFairQueueRemoteFlowControllerTest`.
4. **`BouncyCastleEngineAlpnTest`**, **`NioUdtByteRendezvousChannelTest`**
   (the latter plausibly needs a native UDT library this host doesn't have
   — check before treating as a defect).
5. **`HashedWheelTimerTest`** — see regressions below, it's both a
   still-open class on G1 (was already non-passing there) and an apparent
   new fail on default/ZGC.

## Three apparent regressions on default and ZGC (not G1) — likely environment, not code

All three were `PASS` in the Aug 16 baseline and are not-`PASS` here. Root
cause was checked for all three; none look like genuine CratonVM
regressions:

- **`CloseNotifyTest`**: `PASS → ABORTED`. `@@RESULT found=4 started=4 ok=2
  aborted=2`. The two aborts are both `Assumption failed: OpenSSL is not
  available`. This class needs `netty-tcnative-boringssl-static` resolvable
  at runtime; something about this rerun's environment didn't have it
  available for half the parameterizations. Not examined further — check
  the classpath/native-lib resolution in the new worktree before assuming
  a VM defect.
- **`SslErrorTest`**: `PASS → NOTESTS` (`found=0 started=0`). Same
  OpenSSL-availability suspect as `CloseNotifyTest` — if this class's test
  methods are parameterized over available `SslProvider`s and none were
  available, zero tests would be generated rather than one being aborted.
- **`HashedWheelTimerTest`**: `PASS → FAIL`, one method,
  `testExecutionOnTime`: `Timeout + 100000 delay 682 must be 125 < 650`.
  A wall-clock timing assertion — classic host-load flakiness, not a
  functional defect. Re-run alone on a quiet host before trusting either
  reading.

**None of these reproduced on G1** (G1 had zero `PASS→non-PASS`
transitions), which is consistent with an environment cause common to the
default/ZGC binaries specifically (built in the same batch, run
back-to-back) rather than a GC-specific code path.

## Repro

```bash
cd apps/netty-suite-runner   # worktree /data/cvm-netty-rerun-fhc-20260817
CV_BIN=bin/cratonvm-netty-fhc20260817-default bash run-netty-suite.sh \
  --list rerun-fhc-classes.txt --gc default --shards 1 --out runs/default-fhc-20260817
# swap --gc g1 / zgc and the matching CV_BIN for the other two collectors
```

## Related

- The dozen 2026-08-16 docs cross-referenced above — this rerun mostly
  reconfirms them at a fresher commit rather than finding new defects in
  those clusters.
- `netty-nonpassed-latest.txt` / the original 2026-08-16 full-suite run —
  source of the 101-class input list.
