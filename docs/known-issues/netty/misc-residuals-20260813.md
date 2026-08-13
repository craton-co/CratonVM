# Misc non-TLS residuals — genuine CratonVM-specific FAILs, ungrouped

**Status:** OPEN, not root-caused, no shared cause between these classes
(2026-08-13). Found on Windows, commit `ae2e1d9c8`, isolated (`--shards 1`,
`--timeout 180`), same classpath both VMs. Grouped only because none fit
the other same-day docs' themes (DNS, BouncyCastle, TLS discovery, TLS
cert-validation) — treat each as independent.

## Data

| class | CratonVM | HotSpot |
|---|---|---|
| `buffer.search.SearchProcessorTest` | FAIL 14 ok / 1 fail (15 found) | PASS 15/15 |
| `channel.unix.NativeInetAddressTest` | FAIL 1 ok / 1 fail (2 found) | PASS 2/2 |
| `handler.codec.http2.Http2MultiplexTransportTest` | ABORTED 5 ok / 2 aborted / 4 skipped (11 found) | PASS 7 ok / 0 aborted / 4 skipped |
| `util.internal.JfrEventSafeTest` | FAIL 2 ok / 1 fail | PASS 3/3 |
| `util.concurrent.DefaultThreadFactoryTest` | FAIL 4 ok / 1 fail | ABORTED 3 ok / 2 aborted |

`Http2MultiplexTransportTest`: both VMs find the same 11 tests and skip the
same 4 (`skipped=4` on both), but CratonVM additionally aborts 2 that
HotSpot runs successfully — a narrower gap than the others (2 tests, not a
whole-class failure).

`DefaultThreadFactoryTest` is the odd one out: CratonVM actively fails a
test that HotSpot merely skips via assumption (`aborted`, not `ok`) — same
FAIL-vs-ABORTED asymmetry seen in `PemEncodedTest` in the TLS-residuals
doc, worth checking whether it's the same assumption-handling gap.

## Not yet done

No raw logs read for any of these beyond the harness's auto-extracted `sig`
column (which was empty for all five — the harness's signature grep didn't
match anything useful in these particular failures). Each needs its own
raw-log read before a real root-cause claim is possible.

## Repro

```bash
cd apps/netty-suite-runner
printf '%s\n' io.netty.buffer.search.SearchProcessorTest io.netty.channel.unix.NativeInetAddressTest io.netty.handler.codec.http2.Http2MultiplexTransportTest io.netty.util.internal.JfrEventSafeTest io.netty.util.concurrent.DefaultThreadFactoryTest > /tmp/misc.txt
CV_BIN=bin/cratonvm-netty-zgc.exe bash run-netty-suite.sh --list /tmp/misc.txt --gc zgc --shards 1 --timeout 180 --out /tmp/repro
bash run-netty-suite.sh --list /tmp/misc.txt --hotspot --shards 1 --timeout 180 --out /tmp/repro-hs
```
