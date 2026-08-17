# `DnsNameResolverTest` / `SearchDomainTest` — `localhost` was resolved twice, and the two resolvers disagreed

**Status: FIXED 2026-08-17**, branch `fix/netty-dns-ftl-zgc-20260817`. Closes
both `known-issues/netty/dnsnameresolvertest-searchdomaintest-hang-fail-20260816.md`
(the 08-16 page that narrowed the symptom to the destination address
`0.0.0.1`) and `dns-searchdomaintest-and-dnsnameresolvertest-regressed-20260813.md`
(the 08-13 page that first caught the Windows-only regression and proposed the
`selector_register` epoll-refresh hypothesis). The 08-13 hypothesis was wrong;
the 08-16 page's `0.0.0.1` lead was right, and this is where it goes.

## Result

Windows host, isolated (`--shards 1`), `--gc g1`, one class per process:

| class | before | after the address fix | after all four fixes | HotSpot 25 |
|---|---|---|---|---|
| `SearchDomainTest` | FAIL 1/7 | **PASS 7/7** | **PASS 7/7** | PASS 7/7 |
| `DnsNameResolverTest` | **HANG**, no `@@RESULT` at the 180 s cap | 220 ok / 4 failed / 8 aborted | 232 found / **224 ok / 0 failed** / 8 aborted | 232 / 224 / 0 / 8 |

The 8 aborted are the same environment-gated `Assumption` skips HotSpot
reports, and 224/232 is exactly HotSpot's score. The 4 failures the address fix
left behind were three further defects, all below; each one only became visible
once the one above it was gone.

## Root cause: the resolved address lost a race with the hostname

`dc_socket_addr` (`native-io/src/lib.rs`) rendered a Java `InetSocketAddress`
as `host:port` for `bind`/`connect`/`send`, and it answered
`InetSocketAddressHolder.hostname` FIRST — then `InetAddressHolder.hostName` —
reaching the numeric `address` int only when both were absent. So a
`new InetSocketAddress("localhost", 0)`, whose `InetAddress` the JVM had
**already resolved to 127.0.0.1**, was handed to `UdpSocket::bind` as the
string `"localhost:0"`.

That asks the platform resolver the same question a second time, and the two
answers are ordered differently:

* Windows `getaddrinfo("localhost")` returns `::1` first.
* glibc, with the stock `127.0.0.1 localhost` line ahead of `::1 localhost`
  in `/etc/hosts`, returns `127.0.0.1` first.

HotSpot never has this divergence: `sun.nio.ch.Net.bind` takes
`isa.getAddress()` and never looks at the name. **That is the whole
Windows-vs-Linux split the 08-13 page recorded** — identical bytes, identical
code, two resolvers.

### How `::1` became `0.0.0.1`

The 08-16 page's central piece of evidence was that every DNS query went to
`0.0.0.1`, "not a value any test in this suite would construct on purpose",
and left the mechanism open. It is Apache MINA, one frame above netty.
`NioDatagramAcceptor.localAddress()` (mina-core 2.2.3) reads
`handle.socket().getLocalSocketAddress()` and then, under its own comment
*"Ugly hack to workaround a problem on linux"*, rewrites any `Inet6Address`
for which `isIPv4CompatibleAddress()` holds into the IPv4 address in its last
four bytes:

```java
byte[] ipV6Address = ((Inet6Address) inetAddress).getAddress();
byte[] ipV4Address = new byte[4];
for (int i = 0; i < 4; i++) { ipV4Address[i] = ipV6Address[12 + i]; }
```

`::1` satisfies `isIPv4CompatibleAddress()` (its first twelve bytes are zero),
and its last four bytes are `[0, 0, 0, 1]`. So netty's `TestDnsServer` — which
binds via `new UdpTransport(address.getHostName(), 0)`, i.e. `"localhost"` —
reported its own address as `/0.0.0.1:PORT`, netty pointed the resolver there,
and every query died with `WSAENETUNREACH`.

Recovered with the netty logging bridge the misc-residuals page recommends
(`InternalLoggerFactory.setDefaultFactory(JdkLoggerFactory.INSTANCE)`), which
is what makes the swallowed `IOException` visible under the
`Future.getNow() is null` NPE `SearchDomainTest` reports.

### The fix

`dc_socket_addr` now prefers the resolved `InetAddress`, via a new
`inet_addr_literal` that renders it numerically — IPv6 read from
`Inet6Address.holder6.ipaddress` **before** the v4 `holder.address` int, since
that int stays 0 for a v6 address and reading it would render every v6 address
as the v4 wildcard. The hostname is used only when there is no resolved
address, i.e. for a genuinely unresolved `InetSocketAddress`.

`native-builtins`' `java.net.DatagramSocket` bind/connect path got the same
rule as `read_socket_address_numeric`, deliberately separate from
`read_inet_socket_address`: that helper answers the NAME first, which is
correct for callers that need a name (an HTTP `Host:` header, TLS SNI) and
wrong only for callers that hand the result to the OS.

## Residual 1: twelve `java.net.DatagramSocket` methods threw `InternalError`

`testAddressAlreadyInUse` failed with

```
java.lang.InternalError: Should not get here
    at java.net.DatagramSocket.delegate(DatagramSocket.java:253)
    at java.net.DatagramSocket.getLocalSocketAddress(DatagramSocket.java:566)
```

On a real-JDK build, `java.net.DatagramSocket`'s bytecode for most of its
surface is `delegate().x()`, and `delegate()` is
`if (delegate == null) throw new InternalError("Should not get here")`. Every
socket the CratonVM registrar builds has a null `delegate` — its `<init>`
intercepts replace the JDK constructor that would set one — so an
**unregistered** method there is not a missing feature or an
`UnsupportedOperationException`. It is a hard `InternalError`, which is why it
reads as an impossible VM state rather than a gap. Same shape as
`an_unregistered_overload_on_an_off_object_engine_reads_as_impossible_state`.

Censused one call per declared method (`javap -p -s java.net.DatagramSocket`,
`probes/DsCensus.java`) against HotSpot 25 on the same host. **Twelve threw**,
and one test had found one of them:

| method | before | after / HotSpot |
|---|---|---|
| `getLocalSocketAddress()` | InternalError | the bound address, null once closed |
| `getRemoteSocketAddress()` | InternalError | null, or the peer after `connect` |
| `getSendBufferSize()` / `setSendBufferSize(int)` | InternalError | 65536 / ok |
| `getReceiveBufferSize()` / `setReceiveBufferSize(int)` | InternalError | 65536 / ok |
| `getTrafficClass()` / `setTrafficClass(int)` | InternalError | 0 / ok |
| `supportedOptions()` | InternalError | the 8 names `setOption`/`getOption` answer |
| `getOption(SO_RCVBUF)` | `UnsupportedOperationException` | 65536 |
| `bind(SocketAddress)` | silent no-op — `isBound()` stayed **false** | binds; `isBound()` true |
| `<init>(SocketAddress)` | ran the JDK ctor, side table empty — `send`/`receive` said "closed" | binds |

Two more divergences the census caught that no test had:

* `getBroadcast()` answered a hardcoded `false` "the JDK default". Measured,
  HotSpot 25 answers **true** — its `DatagramSocket` is a `DatagramChannel`
  adaptor and `DatagramChannelImpl` sets `SO_BROADCAST` at construction. The
  ctors now do the same, so a caller that never touches `setBroadcast` gets
  the socket the JDK would have given it instead of one that silently drops
  broadcast sends.
* `getReuseAddress()` answered a hardcoded `true`; HotSpot answers **false**.
  Both getters now read the real socket when nobody has called the setter —
  `udp_reuse_address` / `udp_broadcast` did not exist when those constants
  were written.

## Residual 2: the channel was IPv4-only, so a timeout was reported as a send failure

`testTimeoutNotCached` points its resolver at
`new InetSocketAddress(NetUtil.LOCALHOST, 12345)` with nothing listening and
asserts a `DnsNameResolverTimeoutException`. It got a plain
`DnsNameResolverException`, and the assertion read as a wrong-exception-type
defect several layers above the cause:

```
java.io.IOException: DatagramChannel.send to [::1]:12345: … (os error 10047)
```

`10047` is `WSAEAFNOSUPPORT`. `NetUtil.LOCALHOST` is `::1` on a dual-stack
host, and CratonVM's `DatagramChannel.open()` created an **AF_INET** socket,
which cannot take an AF_INET6 destination. HotSpot's `Net.socket` picks
`INET6` with `IPV6_V6ONLY` off whenever IPv6 is available, so it sends the
datagram and the query times out as the test expects.

Fixed in three parts, because one alone regresses the other direction:

1. `open_udp_dual_stack` — AF_INET6 with `set_only_v6(false)`, falling back to
   AF_INET when there is no IPv6 stack (the JDK's own fallback);
   `native_dc_open` uses it.
2. `udp_send`/`udp_connect` map a v4 destination to its v4-mapped form on a v6
   socket, which is what `Net.translateToSocketAddress` does — the OS refuses a
   bare v4 sockaddr on a dual-stack socket.
3. `udp_recv`/`udp_local_addr`/`udp_peer_addr` un-map `::ffff:a.b.c.d` back to
   `a.b.c.d`, because a dual-stack socket reports every v4 peer in the mapped
   form and HotSpot's `DatagramChannel.receive()` from a `127.0.0.1` sender
   answers `/127.0.0.1`. Without this, making the channel dual-stack would
   have changed every local round trip's reported peer — a louder regression
   than the bug.

And a fourth that the first three exposed: `DatagramChannel.bind(null)` — and
`bind(new InetSocketAddress(0))`, whose address is the v4 `0.0.0.0` — went
through `udp_rebind` with the literal `"0.0.0.0:0"`, **replacing** the AF_INET6
socket `open()` had just created. Every netty datagram channel binds before
use, so the dual-stack socket never survived to its first send.
`udp_rebind_dual_stack` keeps the wildcard on AF_INET6, matching HotSpot, which
reports `/[0:0:0:0:0:0:0:0]:PORT` for both wildcard forms.

## Residual 3: fixing the channel's family exposed the socket's

With the channel dual-stack and `java.net.DatagramSocket` still AF_INET,
`testAddressAlreadyInUse` changed failure rather than passing:

```
Unexpected type, expected: <java.net.BindException>
                but was: <io.netty.resolver.dns.DnsNameResolverTimeoutException>
```

The test holds a port with `new DatagramSocket()`, points a resolver at
`datagramSocket.getLocalSocketAddress()`, and asserts the resolver's bind
fails. It did not fail: the socket held the **v4** wildcard on that port and
the resolver's channel took the **v6 dual-stack** wildcard, so the two never
collided and the bind succeeded. The query then timed out, which is a
different exception at a different layer.

HotSpot has no such split — its `DatagramSocket` is the same
`DatagramChannel` adaptor, so both ends are AF_INET6 and the second bind
genuinely conflicts. Measured: HotSpot's `new DatagramSocket().getLocalAddress()`
is `/0:0:0:0:0:0:0:0`, and CratonVM's was `/0.0.0.0` — a divergence the
`DsCensus` table above had already recorded as cosmetic. It was not cosmetic.

The wildcard `DatagramSocket` constructors (`()`, `(int)`, and
`(SocketAddress)` when the address is `0.0.0.0`) and `bind(SocketAddress)` now
open dual-stack too, through `open_udp_wildcard_dual_stack_gated` so the
network capability gate still sees the same scope. An **explicit** address
keeps its own family, which is also what the JDK does.

The general lesson: **a family mismatch between two sockets is invisible until
something needs them to collide.** Changing one end's family is not a local
change — it is a change to every "is this port taken" question the process can
ask.

## What this retires

The 08-13 page's live hypothesis — that `native_dc_bind` /
`selector_register`'s `#[cfg(target_os = "linux")]` epoll refresh had no
Windows equivalent — is not the cause. A Windows arm for that exists
(`nudge_blocked_poll`), and the actual mechanism is a resolver disagreement
that has nothing to do with selectors. The page was right that the bug was
Windows-only and right to hold the "genuinely Windows-only path" line; it
named the wrong path.

## Lesson worth keeping

**Never re-resolve a name the JVM has already resolved.** An
`InetSocketAddress` carries both a name and an address; handing the OS the
name asks a second resolver the same question, and two resolvers order
multi-homed answers differently. The platform split that produces is invisible
on whichever platform you develop on. HotSpot's rule — `isa.getAddress()`,
never the name — is the one to copy at every `bind`/`connect`/`send` site.

Corollary for the reader chasing the next one of these: the wrong address was
`0.0.0.1`, which is not `::1` and not `127.0.0.1` and looks like neither. A
frame in the middle (MINA) transformed it. **Print the address at the boundary
you control**, not only where it fails.

## Repro

```bash
cd apps/netty-suite-runner
printf '%s\n' io.netty.resolver.dns.SearchDomainTest io.netty.resolver.dns.DnsNameResolverTest > /tmp/dns.txt
./run-netty-suite.sh --list /tmp/dns.txt --gc g1 --shards 1 --timeout 400 --out runs/repro
```

`probes/DsCensus.java` is the `java.net.DatagramSocket` census; run it on both
VMs and diff. The end-to-end netty repro used while bisecting this
(`TestDnsServer` + one `resolve`, with the JDK logging bridge installed so the
cause is not swallowed) is the fastest way back in.
