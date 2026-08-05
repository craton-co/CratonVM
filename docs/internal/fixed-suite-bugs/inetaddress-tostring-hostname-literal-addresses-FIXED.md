# `InetAddress.toString()` kept a hostName for numeric-literal addresses — FIXED

| | |
|---|---|
| **Status** | ✅ **FIXED / RETIRED** 2026-08-05 (filed 2026-08-04) |
| **Area** | `native-builtins/src/{net_phase_e,phases_early,inet_address}.rs`, `native-builtins/src/phases_late/{net_channels,ssl_security}.rs`, `native-io/src/net.rs` |
| **Original symptom** | `InetAddress.getByName("127.0.0.1").toString()` was `127.0.0.1/127.0.0.1`; HotSpot prints `/127.0.0.1` |
| **Retired by** | branch `fix/inetaddress-tostring-hostname-20260804` |

Filed as the one deliberate non-fix when
`serversocket-bind-null-inetaddress-net-sockets` was retired, on the grounds
that expressing "no hostName" changes what every `InetAddress` mirror in the VM
stores. That was the right call — the change did ripple, and it uncovered four
further defects that had nothing to do with rendering.

## The contract, as measured

`InetAddress.toString()` is
`Objects.toString(holder().getHostName(), "") + "/" + getHostAddress()`, so
whether a name was stored is directly observable. Recorded from HotSpot
25.0.3+9 by `probes/InetAddressHostNameProbe.java`:

| construction | `toString()` |
|---|---|
| `getByName("127.0.0.1")`, `getByName("0.0.0.0")`, `getByName("::1")` | `/127.0.0.1` — **no name** |
| `getByAddress(byte[])`, `getAllByName(literal)[0]` | **no name** |
| accepted peer, client local, channel local/remote, datagram local | **no name** |
| `getByName("localhost")`, `getByName(null)`, `getLoopbackAddress()` | `localhost/127.0.0.1` |
| `getByAddress("myhost.example", bytes)` | `myhost.example/1.2.3.4` |
| the wildcard singleton (`new InetSocketAddress(0).getAddress()`) | `0.0.0.0/0.0.0.0` — **name kept** |
| `new ServerSocket(0).getInetAddress()` | `0.0.0.0/0.0.0.0` — the same singleton, by identity |

**The distinction is not "host equals ip".** `getByName("0.0.0.0")` and the
wildcard singleton have the same IP and opposite answers, because
`Inet4AddressImpl.anyLocalAddress()` explicitly names its singleton. Any
render-time `host == ip` shortcut gets that row backwards, which is why the
decision is made at CONSTRUCTION and nowhere else.

## What changed

`net_phase_e` grew two allocators beside the existing
`alloc_inet_address(host, ip)`:

* `alloc_inet_address_unnamed(ip)` — stores [`NO_HOST_NAME`] (the empty string).
  Used by `getByAddress(byte[])`, the UDP local/peer decoders, the datagram
  origin, the channel peer/local decoders and the SSL loopback stand-in.
* `alloc_inet_address_for_input(input, ip)` — stores a name only when `input`
  is not a numeric literal (handles `[::1]` and `fe80::1%3`). Used by
  `getByName`, `getAllByName`, the real DNS resolver, `InetSocketAddress(String,int)`,
  `Socket.getInetAddress()` and the `getHostString()` fallback.
* `alloc_inet_address(host, ip)` is unchanged and still used where a name
  genuinely exists: the wildcard singleton, `localhost`, `getLocalHost()`,
  `getByAddress(name, bytes)`.

Consumers:

* `populate_inet_holder` writes a genuine **null** `hostName` for the unnamed
  case. An empty String would be non-null, and
  `InetSocketAddressHolder.getHostString()` tests that field for null — an
  empty String there would make `getHostString()` answer `""` instead of the IP.
* `getHostName()` / `getCanonicalHostName()` fall back to the numeric text
  rather than answering `""`.

## The four defects the probe found on the way

None of these is a rendering bug; all were found by diffing against HotSpot.

1. **`InetSocketAddress.equals`/`hashCode` compared host TEXT.** So
   `new InetSocketAddress(getByName("127.0.0.1"), p)` was unequal to
   `new InetSocketAddress(getByName("localhost"), p)` — the same endpoint
   spelled two ways, so a `Map<SocketAddress, …>` keyed by peer could hold both.
   The JDK compares `addr.equals(that.addr)` and falls back to the hostname
   (case-insensitively) only when BOTH sides are unresolved. `hashCode` now
   keys on the same thing, or equal objects would hash differently.
   **Pre-existing** — the two host strings differed before this change too.
2. **`getHostName` has SIX registrations, and the winning pair is on the
   CONCRETE subclasses.** Every mirror is an `Inet4Address`/`Inet6Address`, so
   the subclass registration shadows the `java/net/InetAddress` one. Fixing
   only the latter left `getHostName()` answering `""` while `toString()` was
   already correct — on the same object. Both pairs now share
   `inet_addr_host_name_value`.
3. **`getByAddress(new byte[3])` threw `IllegalArgumentException`**; the JDK
   declares and throws `UnknownHostException`, which is what callers catch.
   **Pre-existing.**
4. **`Socket.getLocalAddress()` built the ABSTRACT `java/net/InetAddress`** in
   the legacy two-slot layout (`native-io/src/net.rs`), so its class failed an
   `instanceof Inet4Address` check AND it claimed a hostName equal to its own
   IP. `native-io` cannot reach `native-builtins`, so both sites now route
   through `InetAddress.getByName`, the JDK's own answer for a literal.
   **Pre-existing.**

## Verification

`probes/InetAddressHostNameProbe.java` — 6 sections, recorded on **HotSpot
first** and byte-stable across two runs. Diff after the fix: **4 lines**, both
of them deliberate (below). Before: every literal-derived row differed.

`probes/ServerSocketNullInetAddressProbe.java` (from the predecessor doc) is the
regression control: its HotSpot diff **halved, from 10 differing keys to 5** —
exactly the five `InetAddress.toString` rows this doc is about — with **zero
`pairOk` violations**, i.e. the earlier GC-safety fix is untouched.

`vm/tests/inet_address_hostname_contract.rs` +
`vm/tests/resources/cratonvm/InetAddressHostNameContract.java` pin the whole
table above, in **both socket modes** — the two modes reach the address
builders through different producers, and defect 2 above reproduced in only one
of them.

### Two deliberate remaining differences

* **`getHostName()` does not cache.** HotSpot writes its reverse-lookup answer
  back into `holder.hostName`, so `toString()` on that object changes
  afterwards (`s6.literal.namePresentAfter`). We would cache the *IP*, flipping
  `toString()` from `/127.0.0.1` to `127.0.0.1/127.0.0.1` for any address an
  internal caller happened to ask the name of — reintroducing this very
  divergence at an unpredictable moment. A stable `toString()` is worth the
  lost mutation. No JDK contract requires it.
* **`DatagramSocket.getLocalAddress()` answers the IPv4 wildcard** where
  HotSpot's dual-stack socket answers the IPv6 one. Pre-existing, unrelated to
  hostName (both sides report `hostNamePresent=false`), and already recorded in
  `serversocket-bind-null-inetaddress-net-sockets-FIXED.md`.

## Probe-design notes worth keeping

* **`getHostName()` MUTATES the address.** It caches the reverse lookup into
  the holder, so calling it inside a per-address description poisons every
  later row that shares the object. The first revision of this probe reported
  the accepted socket's remote address as `kubernetes.docker.internal/127.0.0.1`
  purely because of its own observation order. `describe()` must not call it.
* **A probe's `catch` must name `Throwable`, not the expected type.** Catching
  only `UnknownHostException` let CratonVM's `IllegalArgumentException` escape
  and abort the run two sections early, hiding everything after it. A probe
  measures; it does not assert.
* **Filtering a registry dump by the declaring class you expect hides the
  winner.** `class == "java/net/InetAddress"` excluded the `Inet4Address` rows
  that actually run.
