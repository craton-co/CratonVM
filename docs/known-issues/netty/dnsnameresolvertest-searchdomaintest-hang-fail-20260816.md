# `DnsNameResolverTest` (HANG) / `SearchDomainTest` (FAIL) — the 08-13 regression is still live, and today narrows it to one address

**Status: OPEN, CONFIRMED STILL BROKEN.** Written 2026-08-16/17, commit
`3ef3eb744`, Windows host, `cratonvm.exe` release build (isolated,
`--shards 1`, one class per process — not shard contention). Seen failing on
this same commit inside a full 657-class 3-collector parallel suite run on
generational, G1, and ZGC alike; this page reruns G1 isolated and adds a
HotSpot cross-check plus one new, specific piece of evidence.

## Isolation result (G1, isolated)

```bash
cd apps/netty-suite-runner
printf '%s\n' io.netty.resolver.dns.DnsNameResolverTest io.netty.resolver.dns.SearchDomainTest \
    io.netty.pkitesting.CertificateBuilderTest > /tmp/mygroup.txt
./run-netty-suite.sh --list /tmp/mygroup.txt --gc g1 --shards 1 --out runs/agent-dnscert-20260816
```

| class | status | found | ok | failed | aborted | ms |
|---|---|---|---|---|---|---|
| `DnsNameResolverTest` | **HANG** | 0 | 0 | 0 | 0 | rc=timeout, 180s cap hit, no `@@RESULT` |
| `SearchDomainTest` | **FAIL** | 7 | **1** | 6 | 0 | 16356 |

(`CertificateBuilderTest` also ran in this batch; see the separate doc below
— it is not a regression.)

## HotSpot cross-check (same list, same host, isolated)

```bash
./run-netty-suite.sh --list /tmp/mygroup.txt --hotspot --shards 1 --out runs/agent-dnscert-20260816-hotspot
```

| class | status | found | ok | failed | aborted | ms |
|---|---|---|---|---|---|---|
| `DnsNameResolverTest` | ABORTED* | 232 | **224** | **0** | 8 | 30686 |
| `SearchDomainTest` | **PASS** | 7 | **7** | 0 | 0 | 6442 |

\* HotSpot's class status reads `ABORTED` only because `aborted>0 && failed==0`
(the runner's status rule: `failed>0` → FAIL, else `aborted>0` → ABORTED). The
8 aborted methods are ordinary `org.assertj...AssumptionExceptionFactory`
assumption skips (environment-gated parameterizations), not failures — 224/232
run and pass, 0 failed, in 30.7 s.

**This settles the "is it this host's network" question for both classes.**
HotSpot completes `DnsNameResolverTest` cleanly in 31 s and passes
`SearchDomainTest` 7/7 in 6 s, on the same host, same moment, same DNS/network
environment CratonVM ran in. Both suites resolve against netty's embedded
`TestDnsServer` (loopback, ephemeral port) rather than real external DNS, so
this was never expected to be network-reachability-sensitive in the first
place — the HotSpot run confirms it isn't. **Both failures are
CratonVM-specific.**

## New evidence: CratonVM is sending DNS queries to `0.0.0.1`, not loopback

Every `DnsNameResolverTest` DNS-query failure in the isolated raw log carries
the same underlying `IOException`, always to the same non-routable address:

```
Caused by: java.io.IOException: DatagramChannel.send to 0.0.0.1:54104: Сделана попытка
    выполнить операцию на сокете при отключенной сети. (os error 10051)
    at io.netty.channel.socket.nio.NioDatagramChannel.doWriteMessage(NioDatagramChannel.java:307)
    at io.netty.channel.nio.AbstractNioMessageChannel.doWrite(AbstractNioMessageChannel.java:143)
```

(`os error 10051` is Windows `WSAENETUNREACH`; the destination port varies
run to run — 54104, 54116, 54138, 63057, 65319, ... — but the **address is
always exactly `0.0.0.1`**, seen identically across 9 separate query attempts
in this one run.) `SearchDomainTest`'s failures don't repeat the raw send
error text, but show the matching downstream symptom — the resolve future
never completes with a value:

```
java.lang.NullPointerException: Cannot invoke "java.util.List.iterator()" because
    the return value of "io.netty.util.concurrent.Future.getNow()" is null
```

`0.0.0.1` is not a value any test in this suite would construct on purpose —
netty's `TestDnsServer` binds to loopback (`127.0.0.1`) and the resolver is
pointed at whatever local address/port the server actually bound to. `0.0.0.1`
reads as `127.0.0.1` with its leading octet dropped (`[127,0,0,1]` →
`[0,0,1]`, then re-padded to 4 bytes as `[0,0,0,1]`), though this page does not
chase that hypothesis further — it's a lead, not a confirmed cause, and the
existing regressed doc's Windows-selector-refresh hypothesis (below) is a
different mechanism that could produce the same downstream symptom (queries
never get a reply) without necessarily explaining the exact destination byte
pattern. Either way, the query destination itself being wrong is a strictly
more specific, more actionable fact than "inbound datagrams never arrive" was.

## Same shape as the 08-13 "regressed" doc — not a new bug, still open

`fixed-suite-bugs/netty/dns-searchdomaintest-and-dnsnameresolvertest-regressed-20260813.md`
already established, on 2026-08-13:

- `SearchDomainTest` back to **1/7** (matches today's `1/7` exactly).
- `DnsNameResolverTest` not completing even at 400 s (today: still not
  completing at the suite's 180 s cap — worse or equal, not better).
- A Linux re-measurement on the same code (`92d679ba1`) showed the fix
  **holding** there (`SearchDomainTest` 7/7, `DnsNameResolverTest` 196/216 in
  ~85 s), which is what narrowed the earlier page to "genuinely Windows-only
  path" as its live hypothesis, specifically `native_dc_bind` /
  `selector_register`'s `#[cfg(target_os = "linux")]` epoll-refresh code not
  having a Windows equivalent.

Today's run reproduces the exact same numbers on the same platform (Windows)
three days later, on a newer commit (`3ef3eb744` vs `ae2e1d9c8`/`92d679ba1`),
confirming this is not a one-off — it is a standing, unfixed, Windows-specific
regression from the 2026-08-13 `NioDatagramChannel.bind()` fix. This page adds
the `0.0.0.1` destination-address evidence as a new, more specific data point
for whoever bisects/fixes it next; it does not itself bisect or fix.

## Not yet done

- Bisecting Windows-only behavior between the fix landing and today.
- Confirming whether `0.0.0.1` originates from a truncated `127.0.0.1` (byte
  handling) or from something else entirely (e.g. an uninitialized/zeroed
  address struct on the Windows send path) — the regressed doc's
  `selector_register` Windows-arm hypothesis and this page's `0.0.0.1` address
  hypothesis are not yet reconciled into one root cause.

## Repro

```bash
cd apps/netty-suite-runner
printf '%s\n' io.netty.resolver.dns.DnsNameResolverTest io.netty.resolver.dns.SearchDomainTest > /tmp/dns.txt
./run-netty-suite.sh --list /tmp/dns.txt --gc g1 --shards 1 --out runs/repro
./run-netty-suite.sh --list /tmp/dns.txt --hotspot --shards 1 --out runs/repro   # control
grep 'DatagramChannel.send to' runs/repro/*/on-real/shard-0/raw.log
```

## Related

- `fixed-suite-bugs/netty/dns-searchdomaintest-and-dnsnameresolvertest-regressed-20260813.md`
  — the page that first caught this regression on Windows (2026-08-13),
  confirmed it does NOT reproduce on Linux at the same commit, and proposed
  the Windows-only `selector_register`/epoll-refresh code path as the live
  hypothesis. Today's run is the same bug reproducing again, not a new one —
  this page should be read as confirming that page's OPEN status, not
  superseding it. The two pages together are the current state of knowledge;
  a fix should close both.
- `fixed-suite-bugs/netty-batch11-inet6-and-sha1-oid-CLOSED-20260812.md`
  — cause 3, which first recorded these two classes' pre-08-13 numbers
  (`SearchDomainTest` 1/7, hang) as the *before* state the 08-13 fix moved away
  from and this page's numbers now match again.
- `fixed-suite-bugs/netty/certificatebuildertest-fail-status-not-a-regression-20260816.md`
  (this session, same run) — the third assigned class; unlike these two, it is
  **not** a regression.
