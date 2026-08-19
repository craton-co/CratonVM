# CLOSED — `DnsNameResolverTest`'s 16 aborts are eight Windows-only tests, and the class is now categorized and timed for it

**Status:** ✅ CLOSED 2026-08-19 on `fix/netty-dns-and-nativeimage-metadata-20260819`
(branched from `dev` at `c1be69779`). Retires
`docs/known-issues/netty/dnsnameresolvertest-improved-20260819.md`, written the
same day, which recorded the improvement but left three things open: whether the
16 `ABORTED` were a defect, whether the original `0.0.0.1`-routing hang was
really fixed, and whether its own retired predecessor had been lost to
file-loss. All three are now answered.

## What the open page asked, and the answers

**1. "Not cross-checked against HotSpot in this pass."** Now cross-checked.
Stock HotSpot 25 on the same host, same classpath:

```bash
cd apps/netty-suite-runner
/data/toolchain/jdk-25/bin/java @common.args -Dcraton.batch=1 \
    CratonRunner io.netty.resolver.dns.DnsNameResolverTest
```

```
HotSpot 25  @@RESULT ... found=232 started=232 ok=216 failed=0 aborted=16 skipped=0 ms=27546
CratonVM    @@RESULT ... found=232 started=232 ok=216 failed=0 aborted=16 skipped=0 ms=118475
```

Identical in every count. A VM cannot be at fault for a number both VMs produce.

**2. The open page called the aborts an "IPv6-availability check". They are not.**
They are a platform gate. Eight test methods in `DnsNameResolverTest` open with

```java
assumeThat(PlatformDependent.isWindows()).isTrue();
```

— `testResolveLocalhostIpv4`, `testResolveLocalhostIpv6`, `testResolveHostNameIpv4`,
`testResolveHostNameIpv6` and their four `testResolveAll*` counterparts
(`DnsNameResolverTest.java:841,850,859,868,955,964,973,982`). Each is an
`@EnumSource` `@ParameterizedTest` over `DnsNameResolverChannelStrategy`, which
has exactly two constants (`ChannelPerResolver`, `ChannelPerResolution`).
8 × 2 = 16. On Linux those sixteen can never run, on any VM, in any environment
— the host's IPv6 configuration has nothing to do with it. The line the open
page quoted (`:982`) is just the first frame the eager listener happened to
print; it is one of the eight, not the only one.

**3. The predecessor page was not lost.** `dnsnameresolvertest-searchdomaintest-hang-fail-20260816.md`
was retired deliberately by `62cd387c1` ("netty: retire the DNS and
DefaultThreadFactory pages"), which deleted it alongside a real VM fix and wrote
`docs/internal/fixed-suite-bugs/netty/dns-localhost-bind-family-and-datagramsocket-doors-FIXED-20260817.md`
in its place. The `0.0.0.1`-routing defect it described is genuinely fixed, and
the sibling class from that page is clean too:

```
CratonVM  @@RESULT io.netty.resolver.dns.SearchDomainTest found=7 started=7 ok=7 failed=0 aborted=0 skipped=0
```

## The residual that was actually still live: the class is 82% of the wall cap

Not a correctness residual — a harness one, and the likely mechanism behind the
original `HANG` characterization. `DnsNameResolverTest` drives 232 tests through
a live in-process DNS server and is simply a long class on CratonVM:

| arm | wall | result |
|---|---|---|
| idle host, isolated | 118s | ok=216 aborted=16 |
| inside a 6-shard, 19-class run | 125s | ok=216 aborted=16 |
| with five CPU burners alongside | 147s | ok=216 aborted=16 |

The flat per-class cap is 180s. A class killed at the cap is recorded `HANG`,
indistinguishable in the results from a real deadlock — which is exactly how
this class was first characterized. Every count above is identical; only the
clock moves. So the fix is a per-class cap, not a VM change:

```
# class-overrides.tsv
io.netty.resolver.dns.DnsNameResolverTest	600	-
```

## And the residual that would have rewritten this page: `categorize` re-flags it forever

`run-netty-suite.sh categorize` split by literal status, so a class whose only
aborts are platform self-skips could never reach `PASS` and landed in
`others.txt` on every run, buying a fresh investigation each time. This class
has now cost three. Netty's runner gains the mechanism
`apps/hib-suite-runner` already had:

```
# known-benign-aborts.tsv   <class> <found> <ok> <aborted>
io.netty.resolver.dns.DnsNameResolverTest	232	216	16
```

`categorize` treats a listed class as pass-equivalent **only on an exact
found/ok/aborted match**, never on the name. Verified both ways:

```bash
./run-netty-suite.sh categorize --list /tmp/one.txt --shards 1
# table says 232/216/16 -> rebuilt passed.txt=1 others.txt=0 reclassified=1
# table says 232/215/17 -> rebuilt passed.txt=0 others.txt=1 reclassified=0
```

A changed abort profile still lands in `others.txt` like any other residual, so
the table cannot mask a regression on a class it lists.

## Disposition

No VM defect at any point in this page's history that is not already fixed and
documented in `dns-localhost-bind-family-and-datagramsocket-doors-FIXED-20260817.md`.
The two live residuals were both in the harness and are both fixed. Nothing here
is open.

## Related

- `dns-localhost-bind-family-and-datagramsocket-doors-FIXED-20260817.md` — the
  real VM fix that retired this page's predecessor.
- `dns-searchdomaintest-and-dnsnameresolvertest-regressed-20260813.md` — the
  earlier round of the same two classes' history.
- `nativeimagehandlermetadatatest-harness-module-scope-FIXED-20260819.md` — the
  other harness gap closed in the same change.
