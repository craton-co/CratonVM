# spring-boot-mongodb 3-class residuals — FIXED

**Resolved: 2026-07-23**

Follow-up to `mongodb-dns-resolver-null-nameservers-npe-and-reactive-hang-FIXED.md`
(2026-07-18). That fix left `module/spring-boot-mongodb` fully green (23/23,
15/15, 20/20) in an isolated fixture, but the 2026-07-23 full-suite rerun
(`apps/spring-boot-suite-runner/RESULTS-20260723.md`) showed the same 3
classes each with exactly 1 residual failure:

- `MongoAutoConfigurationTests.configuresProtocol`
- `PropertiesMongoConnectionDetailsTests.protocolCanBeConfigured`
- `MongoReactiveAutoConfigurationTests.nettyTransportSettingsAreConfiguredAutomatically`

All 3 were confirmed genuine CratonVM regressions/gaps, not shared-host noise
or environment flakiness: each reproduces deterministically in isolation
(single-class run, no shard contention), and the identical test on real
HotSpot JDK 25 — same host, same network — passes 100%.

## Root causes and fixes

1. **DNS TXT-record lookup throws instead of a clean NXDOMAIN** (bugs 1 & 2).
   `sun/net/dns/ResolverConfigurationImpl.{init0,loadDNSconfig0}` published
   an unconditionally **empty** `os_nameservers` string (a deliberate
   simplification from the 07-18 fix, to dodge a `NoClassDefFoundError`
   cascade in Netty's DNS provider). With no nameservers configured,
   `com.sun.jndi.dns.DnsClient` falls back to its own hardcoded default of
   querying `127.0.0.1:53` — and since nothing listens there, the query can
   only time out or fail with a communication error, never the clean
   `DnsWithResponseCodeException` (response code 3 / NXDOMAIN) that
   mongo-java-driver's `DefaultDnsResolver
   .resolveAdditionalQueryParametersFromTxtRecords` specifically tolerates.
   Real HotSpot instead queries the actual OS-configured DNS server (via the
   Windows IP Helper API), which answers even for a nonsense query. Fixed by
   shelling out to `ipconfig /all` and parsing its "DNS Servers" section as a
   pragmatic stand-in for the IP Helper API, so CratonVM's JNDI DNS client
   reaches the same real, responsive server HotSpot does. Falls back to the
   prior empty-string behavior if `ipconfig` is unavailable/unparsable.
   (`native-builtins/src/lib.rs`, `os_dns_nameservers_string`.)

   A secondary, independently-real gap found and fixed along the way:
   CratonVM's UDP sockets never disabled Windows' `SIO_UDP_CONNRESET`
   behavior (an ICMP "port unreachable" reply to an earlier datagram
   otherwise poisons the *next* `recv()` on that socket with a bogus
   `WSAECONNRESET`/os error 10054, even though UDP is connectionless). Real
   JDK's native UDP implementation disables this at socket creation; CratonVM
   now does too, via `WSAIoctl(SIO_UDP_CONNRESET)` on every UDP socket
   opened through `FileDescriptorTable::open_udp`/`open_udp_reuse`.
   (`native-api/src/fd_table.rs`, `disable_udp_connreset`.)

2. **Netty `EventLoopGroup` shutdown race** (bug 3). The 07-18 fix's
   `native_springboot_mongo_reactive_customizer_destroy` requests
   `shutdownGracefully(0, 0, ms)` but doesn't wait for it, to avoid the
   original `awaitUninterruptibly()` hang. `shutdownGracefully` only CASes
   the group into `ST_SHUTTING_DOWN` synchronously — the `ST_SHUTDOWN`
   transition `isShutdown()` observes happens later, asynchronously, on the
   event-loop thread's own run loop. A caller that checks `isShutdown()`
   immediately after `destroy()` returns can observe `false` even though
   shutdown was correctly requested. Fixed by polling for the real
   transition with a 4-second bound (empirically, a group that actually
   attempted Mongo connections takes ~1.6-2.5s to confirm shutdown under
   CratonVM, vs. ~200ms for a freshly-created, unused group — the child
   event loops have live/pending channel state to unwind first). The poll
   uses `ctx.park()` (the VM-cooperative wait also used by e.g.
   `LockSupport.park`), not `std::thread::sleep` — a raw OS sleep here
   starves the event-loop thread of whatever this thread holds while
   blocked, so an earlier attempt with `std::thread::sleep` always lost the
   race and hit the deadline instead of observing the transition.

## Validation

`cratonvm-sb-mongodb-fix0723.exe`, isolated single-class runs, Windows/JDK 25:

| Class | Before | After |
|---|---:|---:|
| `MongoAutoConfigurationTests` | 22/23 | 23/23 |
| `PropertiesMongoConnectionDetailsTests` | 14/15 | 15/15 |
| `MongoReactiveAutoConfigurationTests` | 19/20 (4 reruns, stable) | 20/20 |

Full `module/spring-boot-mongodb` (all 14 test classes, 94 tests): 0 failures.
