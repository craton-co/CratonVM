# Netty non-passed rerun — 3 GC arms, Azure, 2026-08-22

## Status
**MOSTLY EXPLAINED, one residual open.** Run against a **stale Azure binary**
(`/data/cratonvm`, dev @ `175dc17` from 2026-08-18 — 4 days behind
`origin/dev` @ time of run) with some uncommitted local worktree changes I
left untouched rather than risk clobbering another session's WIP. Several of
today's results match fix docs dated *after* the binary's build date, so a
meaningful chunk of this is very likely stale-binary noise rather than
current-`dev` reality. **Recommend rerunning after an `origin/dev`
update+rebuild on that box before trusting any of this as a current-state
signal** — this doc records what a fresh binary needs to still explain or
fix, not a confirmed current-dev defect list.

## Run

`netty-nonpassed-latest.txt` (43 classes), `run-netty-suite.sh --gc
{default,g1,zgc} --shards 1`, `--timeout 180`, launched concurrently via
nohup+disown (SSH session dropped mid-run; underlying processes unaffected).

| GC arm | PASS | FAIL | HANG | ABORTED | NOTESTS | wall |
|---|---:|---:|---:|---:|---:|---:|
| default | 12 | 7 | 19 | 3 | 2 | 73m10s |
| G1 | 11 | 8 | 20 | 2 | 2 | 75m1s |
| ZGC | 12 | 7 | 19 | 3 | 2 | 72m51s |

**GC-independent**: FAIL/HANG/ABORTED sets are nearly identical across all
three arms (G1 has one extra FAIL, `SearchProcessorTest`; `DnsNameResolverTest`
lands as ABORTED on default/ZGC vs HANG on G1 — the only two arm-specific
differences in 43 classes).

## HANG (19-20 classes) — one cluster fully explained, three genuinely unexplained

**11/19 are the `codec.compression.*IntegrationTest` cluster** (`Brotli`,
`Bzip2`, `FastLz`, `JZlib`, `JdkZlib`, `LengthAwareLzf`, `Lz4Frame`, `Lzf`,
`Snappy`, `SnappyJumboSize`, `Zstd`): already fully diagnosed and **not a
bug** — see
[`compression-testhugedecompress-shared-timeout-20260816.md`](compression-testhugedecompress-shared-timeout-20260816.md).
All 11 share one base-class method, `testHugeDecompress`, that builds 256MiB
one byte at a time; even after the real fix that doc identifies (a
`VarHandle` dispatch cost, landed 2026-08-17 — already in this binary), it
still needs ~455-528s solo with no per-method cap. The suite's 180s cap will
never hold this test regardless of further CratonVM improvement here; the
named residual (`MessageDigest.update(byte)` at the native-dispatch floor)
is tracked separately in `performance/vm-per-call-dispatch-cost-RETIRED-20260817.md`.

**`AdaptiveByteBufAllocator{,Growth,UseCacheForNonEventLoopThreads}Test`**
(3 classes): same shape, already tracked as a "180s wall" throughput
characteristic — see
[`adaptivebytebufallocator-searchprocessor-180s-wall-20260816.md`](adaptivebytebufallocator-searchprocessor-180s-wall-20260816.md).

**`JdkSslEngineTest`**: not yet cross-checked against a specific doc this
session; plausibly related to the SSL-natives fix cluster below (binary
staleness) rather than a distinct issue, but not confirmed either way.

**Three genuinely unexplained, zero-output hangs** — distinct from the
above: `HttpHeaderValidationUtilTest`, `HttpResponseStatusTest`,
`FastThreadLocalTest` all show `rc=124 timeout=180s` with **no partial
progress at all** in the raw log (no `@@RESULT`, no visible test-method
activity before the kill) — unlike the compression/allocator clusters,
which visibly run for a while before timing out. This is a different
shape and was not root-caused here. **Worth a dedicated look**: run each in
isolation with a generous timeout and `CRATONVM_DEFAULT_WATCHDOG_SEC` set,
to get a thread dump of where it's actually stuck (per the established
"a profile of a stuck process describes the stall" methodology) rather than
just a bare timeout kill.

## FAIL (7-8 classes) — mostly plausible stale-binary artifacts

`OpenSslKeyMaterialManagerTest`, `SslContextBuilderTest`, `SslHandlerTest`
all have FIXED docs dated **2026-08-20/21** —
[`openssl-key-material-and-engine-residuals-FIXED-20260820.md`](openssl-key-material-and-engine-residuals-FIXED-20260820.md),
[`sslcontext-natives-ignore-a-third-party-spi-FIXED-20260820.md`](sslcontext-natives-ignore-a-third-party-spi-FIXED-20260820.md),
[`sslengineimpl-tostring-throws-on-a-cratonvm-engine-FIXED-20260821.md`](sslengineimpl-tostring-throws-on-a-cratonvm-engine-FIXED-20260821.md)
— all **after** the Azure binary's 2026-08-18 build. Confirmed directly:
none of those three doc files even exist yet in Azure's checked-out
`/data/cratonvm` worktree. These three FAILs are very likely just
re-confirming already-fixed issues, not regressions — but not proven
without a rebuild.

`PemEncodedTest`, `CertificateBuilderTest`: already documented as expected/
not-a-regression statuses —
[`pemencodedtest-aborted-status-not-a-regression-20260816.md`](pemencodedtest-aborted-status-not-a-regression-20260816.md),
[`certificatebuildertest-fail-status-not-a-regression-20260816.md`](certificatebuildertest-fail-status-not-a-regression-20260816.md).

`ResourceLeakDetectorTest`: 1/3 methods failed. A prior doc closed this
class as "slow not hung" (CLOSED 08-17,
[`resourceleakdetector-concurrentusage-is-slow-not-hung-CLOSED-20260817.md`](resourceleakdetector-concurrentusage-is-slow-not-hung-CLOSED-20260817.md))
— today's result is a genuine FAIL (1 method), not the HANG that doc
addressed, so not obviously the same story. Not root-caused here.

**`PcapWriteHandlerTest`**: 24/25 methods pass; the one failure is
`writePcapGreaterThan4Gb()`, a JUnit-internal 120s timeout writing a >4GB
pcap file. A prior doc
([`pcapwritehandlertest-is-not-a-reopening-FIXED-20260817.md`](pcapwritehandlertest-is-not-a-reopening-FIXED-20260817.md))
closed a *different* issue on this same class, before the binary's build
date, so that fix should already be present — this looks like the same
"inherently slow, doesn't fit the cap" shape as the compression cluster
(writing >4GB of data) rather than a new correctness defect, but wasn't
timed/confirmed against that hypothesis here.

`io.netty.buffer.search.SearchProcessorTest` (G1 only): matches the
already-tracked "180s wall" doc above; not investigated as a G1-specific
anomaly beyond noting it only appeared on one of three arms this run.

## ABORTED (2-3 classes) — already expected

`Http2MultiplexTransportTest`, `CloseNotifyTest`, `DnsNameResolverTest`
(default/ZGC only, HANG on G1) — `DnsNameResolverTest`'s ABORTED status is
already confirmed-expected per
[`dnsnameresolvertest-windows-only-aborts-CONFIRMED-20260819.md`](dnsnameresolvertest-windows-only-aborts-CONFIRMED-20260819.md)
(a doc dated after this binary too, so not literally verified against it,
but the underlying cause — Windows-only environment gaps — isn't something
a CratonVM rebuild would change either way).

## Next steps

1. **Update Azure's `/data/cratonvm` from `origin/dev` and rebuild**, then
   rerun this exact 43-class list. Expect the three SSL FAILs and possibly
   `JdkSslEngineTest` to flip to PASS if the staleness hypothesis is right.
2. Root-cause the three zero-output hangs (`HttpHeaderValidationUtilTest`,
   `HttpResponseStatusTest`, `FastThreadLocalTest`) with an isolated,
   watchdog-enabled rerun — these are the one piece of this doc not
   explained by either "known throughput ceiling" or "stale binary".
3. Re-check `ResourceLeakDetectorTest`'s single FAIL and
   `PcapWriteHandlerTest`'s `writePcapGreaterThan4Gb` timeout against a
   fresh binary; if either persists, they need their own investigation
   rather than inheriting an existing doc's closure.

## Related files

- `apps/netty-suite-runner/netty-nonpassed-latest.txt` — the 43-class list
- Azure: `/data/cratonvm/apps/netty-suite-runner/runs/nonpassed-{default,g1,zgc}-20260822/`
