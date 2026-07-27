# MongoDB DNS resolver and reactive auto-configuration hang - FIXED

**Resolved: 2026-07-18**

## Root causes and fixes

- Real-JDK `ResolverConfigurationImpl.stringToList` dereferenced the native
  resolver's null `os_searchlist` and `os_nameservers` fields. The resolver
  natives now initialize both to non-null empty strings and provide the
  platform ephemeral-port range.
- JNDI DNS uses a non-blocking `DatagramChannel`. CratonVM now supplies both
  channel factories, connected UDP read/write and wildcard bind behavior,
  selector registration/readiness, and a source `InetSocketAddress` whose
  real-JDK layout makes DNS reply-address equality work.
- `configuresSslWithBundle` exposed a separate lifecycle residual. Spring
  Boot's Mongo reactive customizer awaited a Netty termination promise that
  could remain incomplete after TLS bootstrap. Its automatic Mongo-only event
  loop now uses a daemon thread factory; its destroy hook requests the usual
  zero-quiet-period shutdown without awaiting that stale promise. User-supplied
  Mongo transport settings retain Spring's original path.

## Validation

Azure JDK 25 fixture, final isolated `r26` binary:

| Class or probe | JIT | `--nojit` |
|---|---:|---:|
| `MongoAutoConfigurationTests` | 23/23 | 23/23 |
| `PropertiesMongoConnectionDetailsTests` | 15/15 | 15/15 |
| `MongoReactiveAutoConfigurationTests` | 20/20 | 20/20 |
| Connected UDP DNS probe | `WROTE=27`, `READ=27` | `WROTE=27`, `READ=27` |

The SSL-bundle method was also run independently in both modes and completed
with no remaining non-daemon event-loop hang.

## Update 2026-07-23 — all 3 classes regressed to FAIL in `craton-rerun-20260723`, two distinct new symptoms (neither matches the original NPE/hang this doc fixed)

Re-triaging the 2026-07-23 rerun batch (`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/`).
All 3 classes validated 23/23, 15/15, 20/20 above are FAILing again, but with
symptoms that do **not** match the null-nameservers NPE or the SSL-bundle
event-loop hang this doc fixed — filed here rather than as new docs since
they clearly sit in the same DNS-resolver/reactive-transport code this doc
already covers, but flagged explicitly as **not root-caused to this doc's
fix regressing**:

1. **`PropertiesMongoConnectionDetailsTests.protocolCanBeConfigured()`** and
   **`MongoAutoConfigurationTests.configuresProtocol()`** — both construct a
   `mongodb+srv://` connection string, which triggers a genuine DNS TXT-record
   lookup via `com.mongodb.internal.dns.JndiDnsClient`/`com.sun.jndi.dns.DnsClient`
   (the "connected UDP read/write" path this doc's fix added). Both now fail
   with `com.mongodb.MongoConfigurationException: Failed looking up TXT record
   for host localhost`, caused by `javax.naming.CommunicationException: ...
   Caused by: java.io.IOException: receive: Удаленный хост принудительно
   разорвал существующее подключение. (os error 10054)` — a real OS-level
   "connection forcibly closed by remote host" on the UDP socket, i.e. an
   actual outbound DNS query left this machine and got reset. This is **not**
   the null-nameservers NPE this doc fixed (that was a pure-Java NPE before
   any packet went out) — the query now genuinely reaches the network layer
   and gets rejected. Plausibly an environment/network-condition issue (this
   suite runner has no reachable DNS server capable of answering a TXT query
   for `localhost`, similar to the Aether-network-hang confound in
   `modifiedclasspath-aether-network-hang-cluster-FIXED.md`) rather than a
   CratonVM regression — but not confirmed either way; a same-scope HotSpot
   run against the same network would settle it.
   Logs: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard5/logs/module_spring-boot-mongodb.org.springframework.boot.mongodb.autoconfigure.PropertiesMongoConne-8370d7f3db13.out.log`,
   `.../module_spring-boot-mongodb.org.springframework.boot.mongodb.autoconfigure.MongoAutoConfigurationTests.out.log`.

2. **`MongoReactiveAutoConfigurationTests.nettyTransportSettingsAreConfiguredAutomatically()`**
   fails a *different* assertion: `assertThat(eventLoopGroup.isShutdown()).isTrue()`
   returns false after the context closes. This directly touches the destroy-hook
   this doc's third fix bullet added
   (`MongoReactiveAutoConfiguration`'s `DisposableBean.destroy()`,
   `apps/spring-boot/module/spring-boot-mongodb/src/main/java/org/springframework/boot/mongodb/autoconfigure/MongoReactiveAutoConfiguration.java:127-130`):
   `eventLoopGroup.shutdownGracefully().awaitUninterruptibly()` is Spring's own
   code (not CratonVM's), and `awaitUninterruptibly()` is documented to block
   the calling thread until the returned `Future` completes — so for
   `isShutdown()` to still read `false` immediately afterward, either the
   `Future` is being marked complete before Netty's `MultiThreadIoEventLoopGroup`
   actually flips its internal state, or the `awaitUninterruptibly()` call
   itself is returning early on CratonVM. Not root-caused this session (would
   need a debugger attached mid-shutdown to see the actual state-transition
   ordering) — flagged as the strongest new-residual candidate since it's a
   genuine functional (non-network) assertion failure, not an environment
   confound.
   Log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard7/logs/module_spring-boot-mongodb.org.springframework.boot.mongodb.autoconfigure.MongoReactiveAutoCon-90a9dc480afa.out.log`.
