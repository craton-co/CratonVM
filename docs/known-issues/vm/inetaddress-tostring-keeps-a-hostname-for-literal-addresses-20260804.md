# `InetAddress.toString()` keeps a hostName for numeric-literal addresses

**Status: OPEN, measured, not fixed. 2026-08-04.**
**Fails:** nothing known — found by diffing
`probes/ServerSocketNullInetAddressProbe.java` against HotSpot 25.0.3+9, and it
is the ONLY remaining difference in that probe's `InetAddress` /
`InetSocketAddress.toString()` lines.

## What happens

HotSpot's `InetAddress.toString()` is
`(hostName != null ? hostName : "") + "/" + getHostAddress()`, and `hostName`
is **null** whenever the address came from a numeric literal or from raw bytes
— there was no name to remember. CratonVM's mirrors always carry a hostName,
because `net_phase_e::alloc_inet_address(ctx, host, ip)` takes both and every
literal-resolving caller passes `host == ip`.

| expression | HotSpot | CratonVM |
|---|---|---|
| `InetAddress.getByName("127.0.0.1")` | `/127.0.0.1` | `127.0.0.1/127.0.0.1` |
| `InetAddress.getByName("0.0.0.0")` | `/0.0.0.0` | `0.0.0.0/0.0.0.0` |
| `InetAddress.getByAddress(new byte[]{0,0,0,0})` | `/0.0.0.0` | `0.0.0.0/0.0.0.0` |
| `new InetSocketAddress("127.0.0.1", p)` | `/127.0.0.1:p` | `127.0.0.1/127.0.0.1:p` |
| `new InetSocketAddress(0)` | `0.0.0.0/0.0.0.0:p` | `0.0.0.0/0.0.0.0:p` ✅ |

The last row is the trap: HotSpot's **wildcard** genuinely does carry
`hostName = "0.0.0.0"`, so "render `/ip` when `host == ip`" is NOT the fix — it
would break the one case that currently agrees. The distinction HotSpot draws is
"was a name ever supplied", which our mirrors cannot currently express.

## Why it was not fixed with the null-bind work

Found while retiring
`docs/internal/fixed-suite-bugs/serversocket-bind-null-inetaddress-net-sockets-FIXED.md`,
but it is a different defect: that one was a GC-safety bug that produced a
**null** address, this one is a populated address that renders differently.
Expressing "no hostName" means storing an empty host in the side table and in
the real-JDK `holder`, which changes what `getHostName()` / `getHostString()`
fall back to for every mirror in the VM — a wide change that needs its own
regression pass, not a rider on a GC fix.

## Next step

Give `alloc_inet_address` an explicit "this address has no hostName" input
(rather than inferring it from `host == ip`, which is wrong for the wildcard),
set it at the `getByName`-literal and `getByAddress` call sites only, and check
`getHostName()` still answers the numeric text through
`inet_addr_field_string_or`'s default — HotSpot's `getHostName()` on such an
address performs a reverse lookup and falls back to the IP text, so the
observable there should not move.

Re-run `probes/ServerSocketNullInetAddressProbe.java` against HotSpot; the five
`str=` differences above are the whole acceptance criterion. Note that probe
also shows two other differences which are **deliberate and must not change**:
the loopback substitution in `socket_channel::advertised_listener_host`, and
CratonVM's channels answering an IPv4 wildcard where HotSpot's dual-stack ones
answer the IPv6 one.
