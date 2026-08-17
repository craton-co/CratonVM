# `SniClientTest` / `OcspClientTest`: one real CratonVM gap, one perf cliff, and a HotSpot-only flake that isn't ours

**Status: OPEN** (SniClientTest's 3-test gap), **OPEN, perf** (OcspClientTest).
Measured 2026-08-16, commit `3ef3eb744`, Windows host, `cratonvm.exe` release
build. Both classes were flagged as "regressions" from a same-day full
657-class 3-collector parallel suite run against the 43-class baseline from
2026-08-14; this page isolates them (`--shards 1`, one class per process, no
other GC variant running concurrently) and cross-checks against HotSpot 25 on
the same host to find out what of that flagged change is real.

## Summary

| class | CratonVM G1 (isolated) | CratonVM ZGC (isolated) | HotSpot 25 (isolated) |
|---|---|---|---|
| `SniClientTest` | 24/27, 3 failed, 40.8s | 24/27, 3 failed, 28.6s | **4/27, 23 failed**, 5.3s |
| `OcspClientTest` | 6/6, 0 failed, **168.5s** | 6/6, 0 failed, **165.8s** | 6/6, 0 failed, 6.5s |

Neither class is a straightforward "CratonVM regressed" story:

* `SniClientTest` genuinely lost 3 of 27 on CratonVM, reproducibly, in
  isolation, identically on G1 and ZGC — that part is real and CratonVM's own.
* But HotSpot fails the *same class* far worse right now (23/27), for a
  completely different, unrelated reason. Reading the two failure counts
  side by side would say "CratonVM is ahead" — that's not a useful reading
  either, since HotSpot's failures are a netty test-suite race unconnected to
  either VM's correctness.
* `OcspClientTest` never actually fails anywhere. It passes on CratonVM in
  every arm — but at 165–169 s against a 180 s per-class timeout, against
  HotSpot's 6.5 s. Under any host contention (today's full-suite run had two
  other GC variants and their own 657-class suites running concurrently) that
  margin is gone and the class times out, which is what "HANG" meant in the
  full-suite table. This is a throughput gap large enough to look like a hang
  the moment the host has any other load on it.

## `SniClientTest` — the real 3/27

All three failures are the same method, `testSniSNIMatcherDoesNotMatchClient`,
at the three parameterizations where `clientSslProvider = JDK`
(`serverSslProvider` = JDK / OPENSSL / OPENSSL_REFCNT — the OpenSSL client
provider combinations do not fail):

```
org.opentest4j.AssertionFailedError: Unexpected exception type thrown,
  expected: <javax.net.ssl.SSLException> but was:
  <io.netty.channel.StacklessClosedChannelException>
    at io.netty.handler.ssl.SniClientTest.testSniSNIMatcherDoesNotMatchClient(SniClientTest.java:83)
```

The test installs an `SNIMatcher` on the client that rejects the server's
hostname, expects the client to surface that rejection as an `SSLException`
from the handshake, and instead sees the channel close without one — the
handshake-failure path never reaches the alert/exception it's supposed to
deliver to the JDK-provider client.

This is the same shape as the `SslHandlerTest` defect fixed this session
(`03a3e2d20`, `9977fff57` — "a wrap that raises cannot also deliver its
alert"): a fatal condition on the JDK provider's wrap path closes the channel
before the exception it should carry gets delivered to the caller. That fix
was scoped to `SslHandlerTest`'s cipher-mismatch scenario specifically; SNI
matcher rejection on the client side goes through a different call path
(`SniClientTest` builds its own client/server pair rather than reusing
`SslHandlerTest`'s harness) and evidently isn't covered by it. Worth checking
whether the same two-line fix (defer the failure until `drained.is_empty()`,
keep answering NEED_WRAP while a failure is pending) applies here, or whether
this call path never reaches `do_wrap` with a queued alert in the first
place — not yet examined.

Not GC-specific: identical 24/27, same 3 methods, on both G1 and ZGC.

## `SniClientTest` — the HotSpot 23/27 is not about either VM

```
io.netty.channel.ChannelException: address already in use by:
  [id: 0x1a5af713, L:local:test]
    at io.netty.channel.local.LocalChannelRegistry.register(LocalChannelRegistry.java:46)
    at io.netty.channel.local.LocalServerChannel.doBind(LocalServerChannel.java:120)
```

`SniClientTest`'s 9 parameterizations all bind a `LocalServerChannel` to the
same address (`local:test`). `LocalChannelRegistry` is a process-wide static
map, and unregistering the previous parameterization's server channel races
its cleanup against the next parameterization's bind. This is a defect (or at
least a timing assumption) in netty's *own* test, independent of which VM
executes it — the correlation with speed is what triggers it: on this host
HotSpot runs the class in 5.3 s (i.e. ~200ms/parameterization) where CratonVM
takes 29–41 s, and less inter-test time means less time for the previous
channel's async unbind to complete before the next bind lands. **This is not
a CratonVM finding** — it is recorded here only because it fully explains the
lopsided 4/27 vs 24/27 raw counts, which would otherwise misread as CratonVM
being ahead of HotSpot on this class.

## `OcspClientTest` — 25x slower than HotSpot, still correct

No failing assertions in any arm; the concern is pure wall time:

| arm | wall time | of a 180s timeout |
|---|---|---|
| HotSpot | 6.5s | 3.6% |
| CratonVM G1 | 168.5s | 93.6% |
| CratonVM ZGC | 165.8s | 92.1% |

`OcspClientTest` builds real cert chains and runs an in-process OCSP
responder per test method; 6 methods in ~166-169s is ~28s/method, against
HotSpot's ~1.1s/method — roughly the same order of magnitude gap documented
elsewhere for other cert/crypto-heavy netty classes this session. Not
profiled yet; candidate costs to check first given the class's shape
(repeated `KeyPairGenerator`/`CertificateBuilder`/OCSP-responder setup per
method) are JCA object churn and the certificate-builder path already flagged
as slow in `CertificateBuilderTest`'s own results.

**Practical effect:** this margin is why the full 657-class 3-collector
parallel run recorded `OcspClientTest` as HANG on G1 and ZGC — it isn't
hanging, it's landing within about 4x of the timeout even running alone, and
any concurrent host load erases the rest of that margin.

## Repro

```bash
cd apps/netty-suite-runner
printf 'io.netty.handler.ssl.SniClientTest\nio.netty.handler.ssl.ocsp.OcspClientTest\n' > /tmp/isolate2.txt
./run-netty-suite.sh --list /tmp/isolate2.txt --gc g1  --shards 1 --out runs/isolate-regr
./run-netty-suite.sh --list /tmp/isolate2.txt --gc zgc --shards 1 --out runs/isolate-regr
./run-netty-suite.sh --list /tmp/isolate2.txt --hotspot --shards 1 --out runs/isolate-regr
```

## Related

- `openssl-key-material-and-engine-residuals-20260813.md` — the same "a wrap
  that raises cannot also deliver" defect family, documented for
  `SslHandlerTest`; the fix landed there (`03a3e2d20`, `9977fff57`) is the
  first thing to check against `SniClientTest`'s call path.
- `resourceleakdetector-concurrentusage-timeout-20260815.md` — same shape of
  finding (a real throughput gap that a `@Timeout`/harness timeout turns into
  a HANG), same caveat about not reading a raw pass/fail count as a VM
  comparison without isolating first.
