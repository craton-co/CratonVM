# `Inet6Address` dropped the scope id — and four more defects behind it — FIXED

| | |
|---|---|
| **Status** | ✅ **FIXED / RETIRED** 2026-08-13 (filed 2026-08-12) |
| **Area** | `native-builtins/src/net_phase_e.rs`, `native-builtins/src/phases_early.rs` |
| **Retired by** | branch `fix/inet6-scope-id-20260813` |

Filed behind netty [batch 04](../../../known-issues/netty/investigate-INDEX.md)'s
`io.netty.channel.unix.NativeInetAddressTest`. Azure Linux host
(`20.80.105.49`), originally reproduced on a binary built from `origin/dev`
`8763197f2`.

## Original symptom

```
NativeInetAddressTest.testLinkOnlyAddressIncludeScopeId
  expected: <fe80:3030:3030:3030:3030:3030:3030:3031%0>
   but was: <fe80:3030:3030:3030:3030:3030:3030:3031>
```

Reproduced with no netty on the classpath:

```java
byte[] linkLocal = { (byte)0xfe, (byte)0x80, '0','0','0','0','0','0','0','0','0','0','0','0','0','1' };
Inet6Address a = Inet6Address.getByAddress(null, linkLocal, 0);
Inet6Address b = Inet6Address.getByAddress(null, linkLocal, 7);
```

| | HotSpot JDK 25 | CratonVM (as filed) |
| --- | --- | --- |
| `a.isLinkLocalAddress()` | `true` | `true` ✅ |
| `a.getScopeId()` | `0` | `0` ✅ |
| `a.getHostAddress()` | `fe80:…:3031%0` | **`fe80:…:3031`** |
| `a.getHostName()` | `fe80:…:3031%0` | **`fe80:…:3031`** |
| **`b.getHostAddress()`** (scope 7) | **`fe80:…:3031%7`** | **`fe80:…:3031`** |

The `%7` row is the one that made this a defect rather than a formatting
nicety: a **non-zero** scope id was dropped too, so a scoped link-local address
round-tripped to a different address. `getScopeId()` itself was right, so the
information existed and was lost on the way out.

**Impact.** Anything that formats or parses a scoped IPv6 address: link-local
peers on a multi-interface host, `NetworkInterface`-derived addresses, and any
config or log line that round-trips `getHostAddress()`. Silent — the address
just lost its interface and then named a different destination.

---

# Resolution (2026-08-13)

The filed defect was real and is fixed. It was also **one of five**, all in the
same family and all invisible until the address family was measured against
HotSpot end to end rather than at the one accessor the failing assertion named.

## The oracle

`probes/Inet6ScopeProbe.java` (sections A–H) and
`probes/NetworkInterfaceProbe.java`, recorded on **HotSpot JDK 25 first** on the
Azure Linux host, then diffed. Every claim below is a row of that diff.

## The five defects

### 1. The scope was dropped by every renderer

`getHostAddress()` was `Ok(Some(inet_addr_field(ctx, this, IA_ADDR)))` — a plain
read of the numeric text — on all three registered classes, and `getHostName()`
and `toString()` each read `IA_ADDR` directly as well. The real
`Inet6AddressHolder.getHostAddress()` is

```java
String s = numericToTextFormat(ipaddress);
if (scope_ifname != null) s = s + "%" + scope_ifname.getName();
else if (scope_id_set)    s = s + "%" + scope_id;
```

Two details of that body are load-bearing and both were missed:

* the numeric branch is gated on **`scope_id_set`, not on `scope_id != 0`**.
  `Inet6AddressHolder.init` sets the flag for any `scope_id >= 0`, so
  `getByAddress(host, addr, 0)` renders `%0` and `getByAddress(host, addr, -1)`
  renders no suffix at all. A `scope_id != 0` test — which is what
  `native-io/src/net.rs` uses for its own bind/connect purposes, and which the
  filed report pointed at as "a working reference in-tree" — gets that pair
  backwards in **both** directions. netty's `NativeInetAddress.address()` passes
  exactly `scopeId == 0` for a link-local peer, which is why the failing
  assertion was the `%0` one;
* the suffix is the interface **name** when there is one, ahead of the numeric
  id.

Fixed by routing all three renderers through one `inet_addr_scoped_text`, which
appends `inet6_scope_suffix`. The numeric text itself stays unscoped everywhere
it is stored, so `getAddress()`, `equals`, `hashCode`, `isLoopbackAddress` and
the `native-io` bind/connect decoders keep parsing a bare `IpAddr`.

`getHostAddress` has **five** registrations across two files; the winning pair
is on the concrete subclasses. Both files now share the same helper, for the
reason the predecessor doc
[`inetaddress-tostring-hostname-literal-addresses-FIXED.md`](../inetaddress-tostring-hostname-literal-addresses-FIXED.md)
records: fixing one registration leaves the answer depending on which duplicate
won the registry slot.

### 2. The scope did not survive construction

The filed report predicted this ("the fix is not only in the formatter"). Two
producers lost it:

* `getByName("fe80::1%1")` — `resolve_host` answers with a bare
  `std::net::IpAddr`, which cannot carry a scope, so `getScopeId()` was **0**
  where HotSpot answers **1**;
* `populate_inet_holder` wrote `scope_id = 0, scope_id_set = 0`
  unconditionally, with the comment "not recoverable from a bare `Ipv6Addr`".

The scope now travels glued to the address text through the existing
ObjectRef-keyed side table (`fe80:0:0:0:0:0:0:1%eth0`) and is split off inside
`inet_addr_get`, so **no existing consumer of that table sees a changed
string**; only `inet6_scope_suffix` reads the other half. `populate_inet_holder`
additionally resolves the scope to a kernel interface index and writes
`holder6.scope_id` / `scope_id_set`, because `Inet6Address.getScopeId()` is
un-intercepted real-JDK bytecode reading exactly those fields.

The side table was chosen over a second side table because it is already a
scanned **and remapped** GC root (`gc_scan_inet_addr_roots` /
`gc_update_inet_addr_refs`); a new `HashMap<ObjectRef, String>` would have
needed the same rooting in every collector path.

### 3. `Inet6Address.hashCode()` used the wrong algorithm

`Inet6Address.hashCode()` is a **wrapping sum** of the four 4-byte groups, each
accumulated as `(component << 8) + ipaddress[i]` over **signed** bytes. CratonVM
XOR-ed the four groups over unsigned bytes. For
`fe80:3030:3030:3030:3030:3030:3030:3031` HotSpot answers `-1911504703` and
CratonVM answered `-827326463`, so an `Inet6Address` used as a map key hashed to
a different bucket than an equal one built by real-JDK bytecode. Not reachable
from the failing assertion; found by putting `hashCode` in the probe.

### 4. `NetworkInterface.isUp()` was false for every interface on Linux

`isUp0` faithfully implements the JNI body (`IFF_UP && IFF_RUNNING`), but
`re8_host_iface_by_name` sourced its flag word from
`/sys/class/net/<if>/flags`, which is the kernel's `dev->flags` and **never
carries `IFF_RUNNING` (0x40)** — that bit is synthesised by `dev_get_flags()`
for `SIOCGIFFLAGS`/`getifaddrs`. Measured on this host: `lo` reads `0x9`, `eth0`
reads `0x1003`. So the conjunction was false everywhere, where HotSpot answers
`true` for all five interfaces. Anything shaped like "find the first
non-loopback interface that is up" found **nothing at all** — and that is what
made the `scope_ifname` half of defect 1 look untestable: the probe could not
obtain a `NetworkInterface` to scope an address with, so it reported "no
interface" rather than a wrong answer. `IFF_RUNNING` is now re-derived the way
the kernel does, from `operstate`.

### 5. `getInetAddresses()` diverged four ways

Measured against HotSpot, `NetworkInterface.getInetAddresses()`:

* returned **unscoped** IPv6 addresses. HotSpot scopes **every** IPv6 address
  reached through an interface, not only the link-local ones: `lo`'s address is
  `0:0:0:0:0:0:0:1%lo` with `getScopeId() == 1`, even though `getifaddrs`
  reports `sin6_scope_id == 0` for it (verified directly against libc). The
  JDK's Linux enumeration reads `/proc/net/if_inet6` and stores the interface
  **index** as the scope for every row. Deriving the suffix from
  `sin6_scope_id` would have left `::1%lo` unscoped;
* left `getScopedInterface()` **null**. It is now written after the carrier
  exists — it cannot be written during address construction, because minting a
  `NetworkInterface` there recurses into the function that mints addresses;
* labelled each address with the machine hostname (or `localhost`), so
  `toString()` was `vm1/fe80:…` where HotSpot prints `/fe80:…`. This is exactly
  the rule
  [`inetaddress-tostring-hostname-literal-addresses-FIXED.md`](../inetaddress-tostring-hostname-literal-addresses-FIXED.md)
  established; this call site was missed by it;
* reported an interface HotSpot does not have (`ens1`, configured but
  address-less), and put `lo` first where HotSpot puts it last. The JDK creates
  a `NetworkInterface` **from an address**, so an address-less interface is not
  in `getAll0()`'s output at all; and it builds its list by prepending, so the
  order is the reverse of the kernel's. The set now matches exactly and the
  IPv4-carrying tail (`docker0`, `eth0`, `lo`) is now in HotSpot's order.

  **The full order still differs and cannot be made to match.** The JDK's Linux
  IPv6 pass reads `/proc/net/if_inet6`, whose row order is a kernel hash-table
  walk; `getifaddrs` returns ascending index. With 14 `veth` interfaces up,
  HotSpot's block was `3422, 2845, 3427, 3424, …` and ours is strictly
  descending. `Enumeration` order is unspecified by the API, so this is a
  difference and not a defect — but it means a probe row naming "the first
  non-loopback interface that is up" will differ between the two, and that row
  is naming whichever one came first, not reporting a bug.

## Verification

| check | dev (`ae2e1d9c8`) | fixed |
|---|---|---|
| `io.netty.channel.unix.NativeInetAddressTest` | **FAIL** | **PASS** |
| `Inet6ScopeProbe` rows differing from HotSpot (of 63) | 26 | **3** |
| `NetworkInterfaceProbe` rows differing from HotSpot | 19 | **0** (byte-identical) |
| netty network slice, 17 classes | PASS=14 FAIL=3 | see below |

The three residual `Inet6ScopeProbe` rows are all one **deliberate,
pre-existing** divergence: `getHostName()` does not write its answer back into
`holder.hostName`, so `toString()` on an address whose name was asked stays
`/addr` where HotSpot flips to `addr/addr`. That decision, its rationale and its
cost are recorded in
[`inetaddress-tostring-hostname-literal-addresses-FIXED.md`](../inetaddress-tostring-hostname-literal-addresses-FIXED.md)
and are unchanged here.

**The filed report listed `toString()` as a symptom of this bug. It is not.**
HotSpot's own `toString()` on a **fresh** object is `/fe80:…%0` — exactly what
CratonVM prints. The report's table had called `getHostName()` first, and that
call *mutates* the object on HotSpot. The predecessor doc's own probe-design
note says this ("`getHostName()` MUTATES the address … `describe()` must not
call it") and it caught this report out anyway.

## What a narrower probe would have missed

Defects 3, 4 and 5 are all invisible to the failing assertion, which reads one
string off one object built one way. Three of the five are on a different
producer (`NetworkInterface`), one is on a different accessor (`hashCode`), and
defect 4 actively **hid** defect 1's interface-scope branch.

## Repro (historical)

```bash
javac -d . Inet6ScopeProbe.java && cratonvm --java-home <jdk25> -cp . Inet6ScopeProbe
cd apps/netty-suite-runner
printf 'io.netty.channel.unix.NativeInetAddressTest\n' > /tmp/one.txt
bash run-netty-suite.sh --list /tmp/one.txt --shards 1 --timeout 300 --bin <cratonvm> --out /tmp/repro
```
