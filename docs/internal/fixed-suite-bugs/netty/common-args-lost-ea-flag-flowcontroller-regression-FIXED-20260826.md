# Netty harness regression: `common.args` lost `-ea`, un-fixing the two flow-controller classes — FIXED

## Status
**FIXED 2026-08-26.** Not a CratonVM defect — a harness-config regression. `-ea`
re-added to `apps/netty-suite-runner/common.args`.

## Context

Ran the complete 657-class netty suite locally (Windows) across G1/ZGC/Generational
after updating to `dev` HEAD `9ba39b4c1`, comparing against the tracked
`netty-nonpassed-latest.txt` (43-class baseline, 2026-08-13). 96 classes regressed
identically across all three GC arms from that baseline's implied PASS set. Most of
those 96 turned out to already be tracked non-CratonVM-bugs (the
`NativeImageHandlerMetadataTest` cluster, `BootstrapTest`/`ServerBootstrapTest
.mustCallInitializerExtensions`, `HashedWheelTimerTest`, `DataCompressionHttp2Test`,
`BouncyCastleEngineAlpnTest` — see `known-issues/netty/not-cratonvm-bugs-consolidated.md`
and the individual FIXED/RETIRED docs it links). Two were not:
`UniformStreamByteDistributorFlowControllerTest` and
`WeightedFairQueueRemoteFlowControllerTest`, both FAIL, both previously reported
**FIXED 2026-08-19** in
[`http2-flowcontroller-ea-and-datacompression-snappy-20260819.md`](http2-flowcontroller-ea-and-datacompression-snappy-20260819.md)
via adding `-ea` to `common.args` (28/6 → 34/0 passing test methods).

## Root cause

`apps/netty-suite-runner/common.args` (generated/untracked, like the classpath +
sysprops it carries) no longer contained `-ea` — `grep -c -- '-ea' common.args` was 0.
Both flow-controller test classes assert via Java's own `assert` statement in
production code paths they exercise (per the 2026-08-19 doc), so without `-ea` those
assertions are silently skipped and the classes fail differently/incompletely instead.

Likely mechanism: `common.args` was regenerated or hand-edited during this session's
port of `run-netty-suite.sh` from the Linux/Azure layout
(`/data/cratonvm/apps/netty-suite-runner`) to the Windows layout
(`C:/craton/CratonVM/apps/netty-suite-runner`) — see the uncommitted diff to
`run-netty-suite.sh` (JDK autodetect, native-path defaults, `abspath` accepting
`C:/...`) done alongside it. The `-ea` line simply didn't make the trip.

## Fix

Added `-ea` as its own line to `common.args` (it's a Java `@argfile`, one token per
line). Verified directly:

```
cratonvm.exe --java-home <jdk25> --Xmx 1500m -XX:+UseG1GC @common.args \
  -Dcraton.batch=1 CratonRunner io.netty.handler.codec.http2.UniformStreamByteDistributorFlowControllerTest
=> found=34 started=34 ok=34 failed=0   (was FAIL without -ea)
```

## Not yet done

- `common.args` is untracked, so this fix is only as durable as the file on disk —
  worth checking whether a `gen-*` script should own producing this file (the way
  `gen-module-args.sh`/`gen-openssl-args.sh` own their own generated files) so a future
  regeneration can't silently drop it again.
- Did not re-run the complete 657-class suite after the fix to get an updated FAIL
  count; only the two directly-affected classes were spot-verified.

## Related files
- `apps/netty-suite-runner/common.args`
- `apps/netty-suite-runner/run-netty-suite.sh`
- [`http2-flowcontroller-ea-and-datacompression-snappy-20260819.md`](http2-flowcontroller-ea-and-datacompression-snappy-20260819.md)
