# The SSL/PEM/JKS + http-client cluster — three defects, none of them SSL

**Status: FIXED.** The 23-class cluster went 17 FAIL → 1 on Windows against a
HotSpot control that passes all 23. The one remaining class is a heap-pressure
case that passes at `-Xmx 8g` and belongs to the already-open GC docket; the
three defects below are fixed and verified.

The cluster arrived looking like a TLS story: `PemContentTests`,
`PemCertificateParserTests`, `PemSslStoreBundleTests`, `JksSslStoreBundleTests`,
`LoadedPemSslStoreTests`, `SslInfoTests`, `SkipSslVerificationHttpRequestFactoryTests`
and every `*ClientHttpRequestFactoryBuilderTests` /
`*ClientHttpConnectorBuilderTests` class failing at once, right after a merge
that carried NIO/zip attribute changes. None of the three causes is in the TLS
stack, and only the third touches sockets at all.

## 1. One classpath directory, enumerated twice

`ClassLoader.getResources("org/springframework/boot/ssl/pem")` answered **5**
URLs where HotSpot answers **3** — `build/classes/java/test` and
`build/resources/test` each appeared a second time.

`--jar <pathing-jar>` expanded the launch jar's manifest `Class-Path` twice:
once in `vm-cli`'s `-jar` bootstrap (`cp = [jar] + manifest.resolve_class_path`)
and again inside `ClassPath::load_jar_data_at_depth`, which grew its own
manifest expansion when pathing jars were taught to work through the general
`ClassPath::new` path. Jars survived it — that function returns early for an
archive already published — but directories were pushed unconditionally.

The suite runner uses a pathing jar for every module whose classpath exceeds
the Windows command-line limit, which is why this was invisible under a plain
`-cp` (measured: `-cp probeout;classpath.jar` gives the correct 3).

It surfaced through Spring Boot's `@WithPackageResources`, whose extension
copies **every** `getResources` hit into a fresh temp root
(`Resources.addPackage` → `Files.copy(source, target)`). The second copy of the
same directory therefore threw `FileAlreadyExistsException`, which
`ThrowingConsumer` rewraps as a bare `RuntimeException` naming the temp file —
so 42 test classes across the suite failed in `beforeEach`, before their first
assertion, with a message that pointed at a PEM file.

`push_classpath_entry` (`classloading/src/class_path.rs`) applies the
deduplication HotSpot's `URLClassPath` already has: it refuses a URL already on
the path, so `java -cp dir;dir` answers `getResources` with ONE URL (measured,
JDK 25). Every entry kind at every push site goes through it, including
`add_path`, so a repeated `-cp` entry and a re-added dynamic URL behave the same
way.

- Probe: `probes/PackageResourcesProbe.java`
- Fixture: build a pathing jar whose manifest `Class-Path` names a directory,
  launch with `--jar`, count the URLs. It is the `--jar` launch that matters —
  the same classpath passed with `-cp` was always correct.

## 2. An object hashed while locked forgot that hash at the unlock

Three lines of Java, `probes/IdentityHashWhileLockedProbe.java`:

```java
Object p = new Object();
synchronized (p) { inside = System.identityHashCode(p); }
afterUnlock = System.identityHashCode(p);
```

| | HotSpot | before | after |
|---|---|---|---|
| `inside` / `afterUnlock` | equal | `2147483647` then `16` | equal |
| `HashMap.put` under the key's own lock, then `put` again | size 1 | size 2 | size 1 |

The identity hash lives in the upper bits of a NEUTRAL mark word. A
`THIN_LOCKED` payload is an owner plus a recursion count and an `INFLATED`
payload is a monitor pointer, so `mark_word_identity_hash` correctly returns
`Err` for both and the heap falls through to the displaced-hash hook — which
answers `0` for an object that had never been hashed before it locked. The VM's
"identityHashCode must never be 0" guard turned that `0` into `i32::MAX` and
returned it. Every locked-then-hashed object in the process therefore shared one
hash, and each of them changed hash at its next unlock.

A `HashMap` keyed by such an object cannot find its own entry: `put` files it
under one hash and `get` looks under another. `TomcatWebServer` parks a
service's connectors in a `Map<Service, Connector[]>` from inside
`LifecycleBase.start()` — `synchronized` on that very `StandardService`. The
lookup in `start()` missed, the service came back with no connectors, and
`Tomcat.getConnector()`, whose documented job is "fabricate a default port-8080
connector if the service has none", added one to the already-running service.

That is why every embedded-Tomcat test reported

```
ConnectorStartFailedException: Connector configured to listen on port 8080 failed to start
```

which reads like a port conflict and is not one. `probes/TomcatWebServerConnectorProbe.java`
prints `connectors=0` after `start()` before the fix and `connectors=1 [0]`
after, against a HotSpot control that always says 1.

`MonitorTable::identity_hash_via_monitor` inflates and displaces the hash into
the monitor — what HotSpot's `ObjectSynchronizer::FastHashCode` does for a
stack-locked object. It is stable for the object's life: nothing here deflates a
LIVE object's monitor, so the displaced hash stays reachable through the same
pointer. `MonitorTable::java_identity_hash` holds the two-branch composition in
one place so the VM accessor and the in-tree tests exercise the same code.

Two Rust fences in `vm/src/threading/monitor.rs`:
`the_identity_hash_of_a_thin_locked_object_survives_the_unlock` (with a mint
that answers a different value per call, so a re-minting implementation cannot
pass it) and `locked_objects_do_not_all_share_one_identity_hash`.

**This one is not a Spring Boot bug.** `hashCode()` on an object inside its own
`synchronized` block is ordinary Java; anything keyed by such an object was
losing entries.

## 3. A layered TLS upgrade inherited a non-blocking socket

```
SSLHandshakeException: handshake read: ... (os error 10035)
  at AbstractClientTlsStrategy.executeHandshake
```

`10035` is `WSAEWOULDBLOCK`. A blocking socket cannot produce it, so the socket
handed to the TLS layer was non-blocking — no further probe needed to establish
that.

`NioSocketImpl` implements `SO_TIMEOUT` by configuring the fd non-blocking and
polling around it, so an ordinary client `Socket` — the shape Apache HttpClient
hands to `SSLSocketFactory.createSocket(Socket, host, port, autoClose)` for a
layered upgrade — arrives at `take_stream_for_tls` non-blocking. The rustls
handshake that takes ownership next is synchronous: it calls `read_tls` and
expects it to wait.

`take_stream_for_tls` (`native-io/src/net.rs`) now restores blocking mode and
drops the fd's non-blocking record as part of the handoff. The new owner has no
other way to know what mode it inherited, and the fd is retired from the `Net`
registry at that same point.

Windows-only in practice: the Azure Linux full-suite runs of 2026-08-02 and
2026-08-05 both had these classes PASS, because the Unix socket shape reaches
the extraction differently.

## Measured

23-class cluster, `apps/spring-boot-suite-runner/.suite/ssl-pem-cluster-20260809.tsv`,
Windows, JIT on, `-Parallel 4`, default `-MaxHeap 2g`:

| run | binary | FAIL/HANG |
|---|---|---:|
| `sslpem-20260809-pre` | dev @ 681b5c1f1 | 17 |
| `sslpem-20260809-post` | + classpath dedup | 11 |
| `sslpem-20260809-post2` | + identity hash | 2 |
| `sslpem-20260809-final` | + blocking handoff | 1 |

HotSpot control on the same host: all 23 PASS.

The one remaining class is **not an SSL failure and not a hang** — see below.

## The last class is heap pressure, and it belongs to the GC docket

`HttpComponentsClientHttpConnectorBuilderTests` still exceeds the 300s budget at
`-Xmx 2g`. It is not stuck; it is grinding. Four arms, same binary, same class:

| arm | result |
|---|---|
| `-Xmx 2g`, JIT | HANG at 300s (17 servers started, each start slower than the last) |
| `-Xmx 2g`, `--nojit` | FAIL 2/28 in 30.5s |
| `-Xmx 8g`, `--nojit` | **PASS 28/28 in 24.3s** |
| `-Xmx 8g`, JIT | **PASS 28/28 in 125.4s** |
| HotSpot | PASS 28/28 in 14.0s |

Giving it headroom makes both failures and the overrun disappear, so neither is
in the TLS or HTTP path. The `-Xmx 2g` `.err.log` says what is actually
happening:

```
[moving-young] fallback #N: reason=unregistered-jit-frame-on-stack | active-safepoint-map-incomplete | compiled-frame-oop-not-published
ERROR cratonvm::gc::guard: young non-moving sweep was about to ZERO a span containing a LIVE (marked) object ... span_head_class_id=65
[GC-ARRAY-GUARD] array_length(non-array): kind_byte=0 class_id=0 elem_byte=0 stored_len=0
```

Every young collection degrades to the non-moving sweep because a live JIT
frame cannot prove a complete rewritable root map, and the sweep then reaches
live objects. `stored_len=0` on a zeroed header is what surfaced as

```
IllegalArgumentException: Private key must be accompanied by certificate chain
  at java.security.KeyStore.setKeyEntry
```

— a `Certificate[]` chain whose header had been zeroed reads back as length 0,
and `setKeyEntry` refuses an empty chain. The second failure
(`WebClientRequestException: Connection closed by peer`) is the client's view of
the server that could not start.

This is the same signature as
`known-issues/springboot/zipcontenttests-gc-pressure-timeout-not-disk-capacity-20260807.md`
and `known-issues/vm/jit-young-heap-exhaustion-after-header-16-20260807.md`.
It is tracked there, not here: nothing in this cluster's three defects touches
it, and no SSL change can fix it.

## Blast radius, measured

Twelve further `@WithPackageResources` / embedded-server classes
(`.suite/wpr-blastradius-20260809.tsv`), CratonVM vs a HotSpot control that
passes all twelve:

- 9 PASS, 3 exceed the 300s budget (`TomcatServletWebServerFactoryTests`,
  `JettyServletWebServerFactoryTests`,
  `CloudFoundryReactiveActuatorAutoConfigurationTests`) — the same throughput
  axis as above, already filed as
  `known-issues/springboot/tomcat-jetty-servletwebserverfactorytests-300s-budget-overrun-20260807.md`.
- `FileAlreadyExistsException`: **0 occurrences** across the run (was every
  `@WithPackageResources` class).
- `listen on port 8080`: **0 occurrences** across the run (was every
  embedded-Tomcat class).

## Affected classes

`core/spring-boot` — `org.springframework.boot.ssl.pem.PemContentTests`,
`PemCertificateParserTests`, `PemSslStoreBundleTests`, `LoadedPemSslStoreTests`,
`org.springframework.boot.ssl.jks.JksSslStoreBundleTests`,
`org.springframework.boot.info.SslInfoTests`.

`module/spring-boot-http-client` — every
`*ClientHttpRequestFactoryBuilderTests` and `*ClientHttpConnectorBuilderTests`.

`module/spring-boot-web-server`, `module/spring-boot-health`,
`module/spring-boot-cloudfoundry` — one class each.

Defects 1 and 2 reach far past this cluster: 42 test classes across the suite
carry `@WithPackageResources`, and every embedded-server test class in the suite
starts a Tomcat.
