# MongoDB `mongodb+srv`/protocol tests: real-JDK DNS resolver NPEs on null nameserver list; reactive autoconfig class HANGs (separate, unconfirmed)

**Status: OPEN — found 2026-07-17**

## Symptom

3 classes in `module/spring-boot-mongodb`, 2 distinct issues.

### Cluster 1 — `DefaultDnsResolver` TXT-record lookup NPEs via `ResolverConfigurationImpl.stringToList(null)` (CONFIRMED root cause)

| Class | Method |
|---|---|
| `MongoAutoConfigurationTests` | `configuresProtocol()` |
| `PropertiesMongoConnectionDetailsTests` | `protocolCanBeConfigured()` |

```
=> com.mongodb.MongoConfigurationException: Failed looking up TXT record for host localhost
   com.mongodb.internal.dns.DefaultDnsResolver.resolveAdditionalQueryParametersFromTxtRecords(DefaultDnsResolver.java:129)
   com.mongodb.ConnectionString.<init>(ConnectionString.java:459)
   org.springframework.boot.mongodb.autoconfigure.PropertiesMongoConnectionDetails.getConnectionString(PropertiesMongoConnectionDetails.java:84)
 Caused by: java.lang.NullPointerException: Cannot invoke "String.split(String)" because "str" is null
   sun.net.dns.ResolverConfigurationImpl.stringToList(ResolverConfigurationImpl.java:71)
   sun.net.dns.ResolverConfigurationImpl.loadConfig(ResolverConfigurationImpl.java:138)
   sun.net.dns.ResolverConfigurationImpl.nameservers(ResolverConfigurationImpl.java:161)
   com.sun.jndi.dns.DnsContextFactory.serversForUrls(DnsContextFactory.java:148)
```

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-mongodb.org.springframework.boot.mongodb.autoconfigure.MongoAutoConfigurationTests.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-mongodb.org.springframework.boot.mongodb.autoconfigure.PropertiesMongoConne-8370d7f3db13.out.log`

Only 1 test method fails per class (out of 20-ish) — the rest use plain
`mongodb://` connection strings that never route through TXT-record
resolution; only the protocol-configured (`mongodb+srv`-style) test methods
do.

### Cluster 2 — `MongoReactiveAutoConfigurationTests` HANG (hypothesis only, NOT confirmed)

`.out.log` is 0 bytes (never reached a JUnit summary). `.err.log` runs
normally from `20:50:01.89Z` and simply stops at `20:50:21.83Z` — no
exception, no crash signature, no thread dump. The tail (186 of 208 lines)
is a growing burst of:

```
WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped ...
  index=1 num_slots=1 class_id=ClassId(2079) class_name=org/springframework/core/$Proxy27 real_field_count=Some(1)
```

against many distinct, newly-allocated `$Proxy27` objects (not one object
probed repeatedly). Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-mongodb.org.springframework.boot.mongodb.autoconfigure.MongoReactiveAutoCon-90a9dc480afa.out.log`

## Root cause

### Cluster 1 (CONFIRMED at file:line)

`native-builtins/src/lib.rs:37394-37432` registers
`sun/net/dns/ResolverConfigurationImpl.{init0,loadDNSconfig0,notifyAddrChange0}`
as no-ops, leaving the real class's `os_searchlist`/`os_nameservers` fields
at their Java-default `null`. The registration comment explicitly documents
the design assumption (lines 37405-37408): "`loadConfig()`'s
`stringToList`/`addressesToList` treat a null input as empty, giving an
empty search list." **That assumption does not hold for this caller**: real
(unmodified) JDK `ResolverConfigurationImpl.stringToList(String)` at
`ResolverConfigurationImpl.java:71` calls `str.split(...)` with no null
guard — a `null` `os_nameservers` throws `NullPointerException`, not an
empty list. The comment's stated justification ("Netty's own resolver falls
back to platform-default nameservers when this courtesy list is empty")
only covers the specific Netty `DnsServerAddressStreamProviders` call path
these natives were originally added for (see
`docs/internal/fixed-suite-bugs/http-client-cluster-redefine-dispatch-fixes-FIXED.md:67-70`)
— it never accounted for the MongoDB driver's separate, synchronous
JNDI-based `DefaultDnsResolver` (`com.sun.jndi.dns.DnsContextFactory` →
`sun.net.dns.ResolverConfigurationImpl.nameservers()`), which hits
`stringToList` directly and unconditionally NPEs.

**Fix direction (not implemented — this task is investigation-only):** make
`init0`/`loadDNSconfig0` populate `os_searchlist`/`os_nameservers` with an
empty string (`""`) rather than leaving them `null`, matching what a real,
unconfigured host's `loadDNSconfig0()` would actually set — `"".split(...)`
returns `[""]`/an effectively-empty list without NPEing.

### Cluster 2 (hypothesis, unconfirmed)

Not proven to share Cluster 1's mechanism — the reactive test's 20 methods
were checked and none set a protocol/`mongodb+srv` property, so none reach
`DefaultDnsResolver`. The `$Proxy27` out-of-bounds noise is consistent with
the same pre-existing, generally-benign `gen_heap::get_field` guard noise
seen throughout this rerun (see `bug-wildfly-get-field-factory-noise.md`),
but here it recurs against a *growing* set of distinct proxy objects rather
than one repeated object — worth a second look, but not established as the
hang's cause. `MongoReactiveAutoConfigurationTests.nettyTransportSettingsAreConfiguredAutomatically()`
(the one method in this class touching a real Netty `EventLoopGroup`
shutdown) is the most plausible trigger among the 20 methods but this is
speculative — no thread dump or blocking-region diagnostic was captured. A
similarly-shaped, already-fixed Netty `EventLoopGroup` shutdown hang exists
(`docs/internal/fixed-suite-bugs/keycloak-model-stw-takeover-hang-eventloopgroup-shutdown-FIXED.md`)
but its signature (`STW cross-thread JIT takeover is still waiting...`)
does not appear anywhere in this HANG's log, so it is at best a
similarly-shaped, unconfirmed possibility — not a re-occurrence of that
exact bug. Needs a live repro with `CRATONVM_SYMBOLIZE=1`/thread-dump
tooling to pin down.

**Documentation check:** no existing doc anywhere in
`docs/internal/fixed-suite-bugs/`, `docs/internal/springboot/`, or
`docs/known-issues/springboot/` references "TXT record",
"ResolverConfigurationImpl", "stringToList", or has any MongoDB-specific
entry — both clusters are new.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-mongodb` | `org.springframework.boot.mongodb.autoconfigure.MongoAutoConfigurationTests` (Cluster 1, 1 of ~20 tests) |
| `module/spring-boot-mongodb` | `org.springframework.boot.mongodb.autoconfigure.PropertiesMongoConnectionDetailsTests` (Cluster 1, 1 of ~20 tests) |
| `module/spring-boot-mongodb` | `org.springframework.boot.mongodb.autoconfigure.MongoReactiveAutoConfigurationTests` (Cluster 2, HANG, unconfirmed) |
