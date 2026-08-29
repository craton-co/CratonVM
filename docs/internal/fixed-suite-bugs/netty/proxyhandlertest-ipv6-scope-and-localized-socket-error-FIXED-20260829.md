# `ProxyHandlerTest` 8 of 47 — FIXED 2026-08-29: a scope suffix nothing could parse, and a socket error message in the wrong language

## Status

**FIXED.** `fix/netty-autoscale-and-npe-residuals-20260829`, commits
`5f89ed5fb` and `79caaf42f`.

| arm | result |
|---|---|
| before | `found=47 ok=39 failed=8`, every run |
| after | `found=47 ok=47 failed=0`, 5/5 runs |
| HotSpot 25, same host and classpath | `found=47 ok=47 failed=0` |

`regression-suite/run.sh` 76/76, including the new differential vector
`RNetIfaceScope`.

## Where this came from, and the sentence that was wrong

`known-issues/netty/unexplained-npes-in-randomized-tests-20260826.md` carried
this class in a section called "Possibly related, uncertain":

> `ProxyHandlerTest` failed 8 of 47 parameterizations this same run with
> `array lengths differ, expected: <0> but was: <1>` (one extra byte arrived in
> an AUTO_READ success-path check) — not an NPE, and plausibly just contention.

It is not contention, and no extra byte arrived. Re-run on a quiet host on this
branch's binary it failed **8 of 47 on every run**, with HotSpot at 47/47 on
the same host. The `expected: <0> but was: <1>` is not a byte count: it is
`assertArrayEquals(EMPTY_OBJECTS, testHandler.exceptions.toArray())` — the
client recorded ONE exception where the test asserts none.

That is worth writing down on its own. The class was cleared as noise on a
reading of the assertion that a look at `ProxyHandlerTest.java:718` would have
corrected, and it then sat clear for three days.

## What the 8 have in common

Sorting the failures by parameterisation name gives the answer before any
debugging does. All 8 are `SuccessTestItem`s, and all 8 — and only they — have
`clientSslCtx.newHandler(...)` in the pipeline:

| failing item | shape |
|---|---|
| Anonymous HTTPS proxy: successful connection, AUTO_READ on/off | TLS + HTTP proxy |
| HTTPS proxy: successful connection, AUTO_READ on/off | TLS + authenticated HTTP proxy |
| Single-chain: successful connection, AUTO_READ on/off | SOCKS5 → SOCKS4 → **TLS** → HTTPS → HTTP → HTTP |
| Double-chain: successful connection, AUTO_READ on/off | the above, twice |

Every `FailureTestItem` and `TimeoutTestItem` with the same TLS handler passes,
because they expect a failure. So: a client-side TLS session over a proxy
connection, on the path where everything is supposed to work.

There turned out to be two defects, stacked. Fixing the first took the count
from 8 to 5-7 and changed *which* assertion failed; only both together give
47/47.

## Defect 1 — the IPv6 loopback carried a scope nothing could parse

The tell was in the log and nowhere in the assertion: eight occurrences of

```
java.net.UnknownHostException: 0:0:0:0:0:0:0:1%{3C307829-8147-11F1-95EB-806E6F6E6963}
	at io.netty.util.internal.SocketUtils.addressByName(SocketUtils.java:151)
	at io.netty.resolver.DefaultNameResolver.doResolve(DefaultNameResolver.java:41)
	…
	at io.netty.handler.proxy.ProxyServer$IntermediaryHandler.connectToDestination(ProxyServer.java:211)
```

eight occurrences, eight failures.

`ProxyServer.address()` is `new InetSocketAddress(NetUtil.LOCALHOST, port)`,
`NetUtil.LOCALHOST` comes out of `NetworkInterface` enumeration, and
`Socks4ProxyHandler` puts `getHostAddress()` of the destination **on the wire
as text**. The proxy at the other end hands that text back to
`InetAddress.getByName`. So the address had to round-trip, and it did not.

`InetScopeProbe`, the two VMs on this host:

| | `NetUtil.LOCALHOST.getHostAddress()` | `getScopeId()` | `getScopedInterface()` | round-trips |
|---|---|---:|---|---|
| HotSpot 25 | `0:0:0:0:0:0:0:1` | 0 | null | yes |
| CratonVM before | `0:0:0:0:0:0:0:1%{3C307829-…}` | 0 | the loopback | **no** |

`re8_make_interface` scoped every IPv6 address to its interface, and said so:

> EVERY IPv6 address reached through an interface is scoped to that interface,
> not just the link-local ones. Measured against HotSpot JDK 25 on this host:
> `lo`'s address is `0:0:0:0:0:0:0:1%lo` with `getScopeId() == 1` …

**That measurement is correct, and it is a Linux measurement.** The JDK's Linux
enumeration reads `/proc/net/if_inet6` and stores the interface index as the
scope of every row, so `::1%lo` really is scoped there even though
`getifaddrs` reports `sin6_scope_id == 0`. The Windows enumeration reads
`sin6_scope_id` from `GetAdaptersAddresses`, which is the interface index for a
link-local address and **0 for the loopback and for global addresses**. One
rule, two platforms, and the rule was right on the one it was measured on.

Two further things fell out of the same code, both visible in the table above:

* `getScopeId()` answered **0 for every address, including the link-local
  ones** — the renderer prefers `scope_ifname` over the numeric id, so nothing
  had ever needed the numeric half to be written, and nothing wrote it.
* Our Windows `NetworkInterface.getName()` is the adapter **GUID**
  (`{3C307829-…}`) where HotSpot synthesises `loopback_0`, `ethernet_32769`,
  `wireless_32768`. That is a real remaining divergence — see below.

The fix gives `Re8HostIface` a per-address scope column, filled from
`sin6_scope_id` on Windows and from the interface index on Linux, and makes the
`%interface` suffix, the `scope_ifname` field and the numeric `scope_id` all
follow it.

### Defect 1b — and the parser could not read what the renderer wrote

Fixing the loopback left the link-local addresses rendering
`fe80:…%{04C70698-…}`, which `InetAddress.getByName` still could not parse:
`Ipv6Addr::from_str` rejects a `%scope` suffix, so `resolve_host` sent the
whole string to DNS. The VM could not read text it had just produced. HotSpot
parses it — a numeric scope as-is, a named one if it names a real interface —
and now so does `resolve_host`.

This one did not affect `ProxyHandlerTest` (its addresses are loopback), and it
is the reason the new regression vector exists: it is the half of the contract
a test that only looks at the loopback cannot see.

## Defect 2 — the socket error message was in the OS's language

With the addresses fixed, 5-7 of the 47 still failed, and now for one reason
only. The client-side recorded exception, with `io.netty.handler.proxy` at
DEBUG:

```
java.io.IOException: SocketException: read: Программа на вашем хост-компьютере
разорвала установленное подключение. (os error 10053)
	at io.netty.buffer.AdaptivePoolingAllocator$AdaptiveByteBuf.setBytes(…)
	at io.netty.channel.socket.nio.NioSocketChannel.doReadBytes(…)
```

`WSAECONNABORTED` on a read during TLS teardown. That is a **normal** event —
netty expects it and swallows it. `SslHandler.ignoreException` has two ways to
recognise it, and CratonVM defeated both:

1. It runs
   `^.*(?:connection.*(?:reset|closed|abort|broken)|broken.*pipe).*$`
   over `getMessage()`. `std::io::Error`'s `Display` is `FormatMessage`
   output, i.e. **localized** — on this Russian-locale Windows the message
   cannot match an English regex. HotSpot says "Software caused connection
   abort".
2. Failing that, it walks the stack for a frame whose method is `read` in a
   class whose name contains `SocketChannel`. On HotSpot that finds
   `sun.nio.ch.SocketChannelImpl.read`. **Our `SocketChannelImpl.read` is a
   native and leaves no Java frame**, so the walk sees only `io.netty.*`
   frames, which it skips by design.

So a routine close surfaced through `exceptionCaught`, the test recorded it,
and `assertArrayEquals(EMPTY_OBJECTS, exceptions)` failed.

`map_err` in `native-io/src/socket_channel.rs` already did the right thing for
`ConnectionRefused`, with a comment explaining exactly this hazard, and the
sibling mapper in `net.rs` already did it for `ConnectionReset` and
`ConnectionAborted`. This one mapper had been missed. It now prefixes the
portable phrase and keeps the OS text after it.

**A VM whose `IOException` text changes with the Windows UI language is wrong
independently of netty.** Any `catch`-and-match on "connection reset" — and
there is a lot of it in the wild — is locale-dependent until the message is
the JDK's.

## What is still divergent, and deliberately not fixed here

Our Windows `NetworkInterface.getName()` / `getDisplayName()` are the adapter
GUID; HotSpot synthesises `<type>_<n>` names (`loopback_0`, `ethernet_32769`,
`wireless_32768`, `ppp_32768`, `tunnel_32512`) and uses the adapter description
as the display name. It also enumerates ~60 interfaces to our 7, because it
walks the filter/lightweight pseudo-adapters as well.

Nothing on this page needs that fixed: with 1b in place our own names
round-trip, and neither the name nor the count is what broke anything here.
Reproducing HotSpot's numbering exactly means reproducing its dual enumeration,
which is a Windows-fidelity project rather than a bug fix. It is written down
here because it is measured (`IfaceNameProbe` in the repro directory prints
both sides) and because the GUID is what makes a scoped address look alarming
in a log.

## Repro

```bash
cd apps/netty-suite-runner
cratonvm.exe --java-home <jdk25> --Xmx 1500m -XX:+UseZGC @common.args -Dcraton.batch=1 \
  CratonRunner io.netty.handler.proxy.ProxyHandlerTest
# the address side, on its own, against HotSpot:
cratonvm.exe --java-home <jdk25> -cp <netty-common> InetScopeProbe
```

`-Dlogback.configurationFile=` with `io.netty.handler.proxy.ProxyHandlerTest`
at DEBUG is what prints the client-side recorded exception; at the default
level the test logs it and the harness never shows it, which is why the
localized message stayed invisible through two investigations.

## Related

* `fixed-suite-bugs/netty/unexplained-npes-in-randomized-tests-CLOSED-20260829.md`
  — the page that carried this class as noise.
* `internal/repros/netty-park-cadence-20260829/` — `InetScopeProbe` and
  `IfaceNameProbe` live there with the park-cadence probes from the same
  branch.
* `fixed-suite-bugs/netty/inet6address-drops-the-scope-id-FIXED-20260813.md` —
  the same field, the opposite direction.
