# craton-rerun-20260730 seven-class residual — findings

**Status: 2026-07-30/31.** Covers the 7 classes left non-`PASS` by
`apps/spring-boot-suite-runner/RESULTS-20260730.md` (6 FAIL + 1 HANG).

Every one of the seven passes on real HotSpot JDK 25 with the same classpath
and the same `SbRunner` entry point (verified first, before any investigation),
so none of them is a pre-existing Spring Boot test problem.

| Module | Class | Outcome |
|---|---|---|
| `core/spring-boot` | `Log4J2LoggingSystemTests` | **FIXED** — `StackFrame.getDeclaringClass()` returned `null` |
| `core/spring-boot` | `ConfigDataEnvironmentPostProcessorIntegrationTests` | **NOT A VM BUG** — stale fixture file in the working directory |
| `module/spring-boot-mongodb` | `PropertiesMongoConnectionDetailsTests` | **FIXED** — empty DNS resolver config when the VM has no console |
| `module/spring-boot-mongodb` | `MongoAutoConfigurationTests` | **FIXED** — same root cause |
| `module/spring-boot-security-saml2` | `Saml2RelyingPartyAutoConfigurationTests` | **FIXED** — HttpURLConnection keep-alive pool never closed idle sockets |
| `core/spring-boot` | `OriginTrackedYamlLoaderTests` | **NOT A HANG** — completes in 345s standalone |
| `module/spring-boot-ldap` | `EmbeddedLdapAutoConfigurationTests` | **OPEN, accepted** — Windows-only legacy-DSA TLS gap |

---

## 1. `Log4J2LoggingSystemTests` — 61/61 failed: `StackWalker$StackFrame.getDeclaringClass()` returned `null`

```
java.lang.NullPointerException: Cannot invoke "Object.equals(Object)" because the return value of
"java.lang.StackWalker$StackFrame.getDeclaringClass()" is null
   org.apache.logging.log4j.util.StackLocator.lambda$getCallerClass$8(StackLocator.java:71)
   ...
   org.springframework.core.env.AbstractEnvironment.<init>(AbstractEnvironment.java:103)
```

Every test failed in `@BeforeEach` (`new MockEnvironment()`), so the whole class
was lost to one defect. Reproduced identically on Linux and Windows.

**Root cause.** There are two different `StackWalker` frame carriers in this VM:

* `lang_stackwalker::populate_sfi` builds a 6-slot `java/lang/StackFrameInfo`
  whose slot 5 holds the declaring class's *internal name*;
* `phases_late::reflect_invoke::populate_stack_frame` builds an 8-slot
  `java/lang/StackWalker$StackFrame` whose **slot 6 holds the declaring class's
  `Class` mirror**, resolved eagerly from the frame's own `ClassId`.

`StackWalker.walk` produces the second kind. But `lang_stackwalker` also
registers its own `getDeclaringClass` for `java/lang/StackWalker$StackFrame`,
and that registration wins — shadowing `reflect_invoke`'s slot-6 accessor. The
shadowing implementation only knew about the *name* in slot 5 and re-derived a
class from it through `ClassManager::find_unique_class_by_name`, which answers
`None` whenever two class loaders each hold a copy of that name. That is the
normal state under Spring Boot's `@WithResource` / `ClassPathOverrides` forked
loaders (see `reference_multiple_class_copies_are_normal_under_isolating_loaders`),
so the lookup failed and the accessor returned `null` — the eagerly-captured,
loader-exact mirror sitting in slot 6 was never consulted.

**Fix** (`native-builtins/src/lang_stackwalker.rs`): `declaring_class_native`
now prefers the per-frame mirror — `classOrMemberName` read through the
object's own layout, then the p59 carrier's slot 6 — before falling back to the
by-name lookup, with a `Class`-mirror type check on both reads. That check is
what makes one accessor safe for all three carriers (the 6-slot synthetic has
no slot 6; a real-JDK `StackFrameInfo` holds its `ste` there; a real
`ClassFrameInfo` can hold a `ResolvedMethodName` in `classOrMemberName`).

**Two sessions found this independently.** A concurrent session landed the same
repair on `dev` as `1660283ca4` ("a frame's declaring class comes from the
frame, not a by-name lookup") while this branch was in flight. On merge the
conflict was resolved in favour of **dev's version**, which is strictly
stronger: it also enforces the `RETAIN_CLASS_REFERENCE` contract on the
package-private `declaringClass()` bridge, and it gates the slot-6 read on the
carrier actually being the p59 `StackWalker$StackFrame` rather than on a field
count. The verification numbers below were measured against this branch's
equivalent implementation.

**Verified**: 61/61 pass on Linux (where HotSpot is also 61/61).

**On Windows, CratonVM now matches HotSpot exactly**: 61 tests, the *same* 14
methods fail on both VMs (`applicationName*`, `applicationGroup*`,
`correlation*` — console assertions missing the pattern segment, and
`NoSuchFileException` on the file appender's `log4j2-test.log`). Those 14 are a
pre-existing Windows problem with this checkout's fixture/resources, **not a
CratonVM defect** — do not chase them as one. Before the fix all 61 failed on
Windows, because the NPE hit `@BeforeEach`.

---

## 2. `ConfigDataEnvironmentPostProcessorIntegrationTests` — not a VM bug

20 of 87 failed, 19 of them reporting `but was: "fromlocalfile"`. The first
failure explains all the rest:

```
Expecting file:
  ...\apps\spring-boot\core\spring-boot\.\application.properties
not to exist
   ...runWhenHasLocalFileLoadsWithLocalFileTakingPrecedenceOverClasspath(...:358)
```

That test writes `./application.properties`, runs, and deletes it in a
`finally`. A **previous run that was killed before its `finally`** left the file
behind (dated 2026-07-29 in the checkout). From then on the test failed at its
own `assertThat(localFile).doesNotExist()` precondition — *before* entering the
`try`, so the cleanup never ran again and the file was never removed. Every
later test in the class then picked the leftover file up as a config source.

`File.delete()` itself is fine: a standalone probe covering the exact shape
(`new File(new File("."), name)`, write via `FileOutputStream` in
try-with-resources, delete; plus a `Files.delete` and an explicit-`close`
variant) behaves identically on CratonVM and HotSpot.

Deleting the stale file makes the class pass **87/87** on Windows CratonVM. It
also passes on Linux, where the file never existed.

*Harness note:* a killed run of this class can poison every subsequent run in
the same checkout. Worth having the suite runner delete a stray
`core/spring-boot/application.properties` before the class runs.

---

## 3 & 4. Both MongoDB classes — empty DNS resolver config whenever the VM has no console

```
com.mongodb.MongoConfigurationException: Failed looking up TXT record for host localhost
  ... Caused by: javax.naming.CommunicationException: DNS error
      [Root exception is java.net.SocketTimeoutException]
```

`protocolCanBeConfigured` / `configuresProtocol` build a `mongodb+srv://`
connection string, which makes the driver perform a real JNDI DNS TXT lookup.

This looked intermittent for a long time — it failed under the suite runner and
in scripted batches, but passed when run by hand. An **interleaved A/B run**
(same class, alternating VMs, seconds apart) settled it: CratonVM 10/10 FAIL,
HotSpot 10/10 PASS. A long-running watcher that logged
`sun.net.dns.ResolverConfiguration.open().nameservers()` alongside each attempt
then showed the mechanism directly — CratonVM reported `ns=[]` on every failing
cycle while HotSpot reported `ns=[192.168.1.1]` on every passing one.

**Root cause.** `net_uri_inet::os_dns_nameservers_string_uncached` obtained the
nameserver list by forking **`ipconfig /all`** and parsing its output. When the
VM is launched from a **console-less parent** — a hidden-window process, a
service, or any `ProcessStartInfo` with redirected stdio, which is exactly how
`apps/spring-boot-suite-runner` starts every class — that fork yields nothing
usable. The probe then answers "no nameservers", and because the answer is
latched in a `OnceLock` the VM runs with an empty resolver config for the rest
of its life.

An empty list is not inert. JNDI's `DnsContextFactory.serversForUrls` falls back
to the literal string `"localhost"` when `platformServers.isEmpty()`, so every
lookup went to `127.0.0.1:53`, where nothing listens, and burned the full
retry ladder (~15 s) before throwing `SocketTimeoutException`.

Confirmed deterministic: the same one-cycle probe returns
`ns=[192.168.1.1]` + `OK` from an ordinary shell and `ns=[]` + `FAIL` when
launched from a hidden-window PowerShell.

**Fix** (`native-builtins/src/net_uri_inet.rs`): read the servers from the
Windows IP Helper API (`iphlpapi!GetNetworkParams`) — the same API the real
JDK's `loadDNSconfig0` uses. No process spawn, no console, no locale-dependent
label text. The `ipconfig` parse is retained as a fallback so a configuration
this API does not report (e.g. IPv6-only) behaves no worse than before.

**Verified**: from the console-less launcher that previously produced
`ns=[]` + a 15 s failure, the same probe now reports
`ns=[192.168.1.1]` and succeeds in 210 ms.

*Note:* the UDP/selector layer itself was never at fault — a faithful replica of
`com.sun.jndi.dns.DnsClient.blockingReceive` (connected `DatagramChannel`,
shared `Selector`, `receive`, sender-address comparison) measured **identical**
behaviour on both VMs over 300 queries, including losing the same single packet
at the same iteration.

---

## 5. `Saml2RelyingPartyAutoConfigurationTests` — 6/21 failed: pooled keep-alive sockets were never closed

```
java.lang.AssertionError: Gave up waiting for queue to shut down
   mockwebserver3.MockWebServer.close(MockWebServer.kt:417)
```

`MockWebServer.close()` closes its listening socket and then waits up to 5 s per
active `TaskQueue` for its idle latch. A per-connection task is parked in a read
on an accepted socket, so the latch only fires once the *client* hangs up.

**Root cause.** CratonVM's `HttpURLConnection` keep-alive pool
(`native-builtins/src/http_url_connection.rs`) returns a connection to the pool
after a poolable response and **never closes it**: `pool_take` is the only thing
that discards a stale entry, so a client that makes one request and then stops
holds its socket open for the entire life of the VM. The real JDK does not
behave that way — `sun.net.www.http.KeepAliveCache` runs a "Keep-Alive-Timer"
daemon thread that closes idle connections.

Measured with a raw-socket probe (server writes a `Content-Length` response,
then blocks on a read and reports when EOF arrives):

| VM | Connection closed by client |
|---|---|
| HotSpot JDK 25 | after **5.004 s** |
| CratonVM | **still open after 12 s** |

Isolated 1-second reproduction: `MockWebServer` + one enqueued response +
`URLConnection.getInputStream()` read to EOF + `server.close()`. HotSpot closes
in 7 ms; CratonVM throws `Gave up waiting for queue to shut down`. Calling
`HttpURLConnection.disconnect()` explicitly does not help either.

**Fix**: a pool reaper thread (`pool_start_reaper`) that sweeps entries past
`POOL_IDLE_WINDOW` and closes them, started on `pool_put` and exiting once the
pool drains, so an idle VM carries no extra thread. Covered by a new regression
test, `pooled_connection_is_closed_once_idle`, which asserts from the peer's
side — the side that noticed.

---

## 6. `OriginTrackedYamlLoaderTests` — not a hang

Reported as HANG at the suite's 1500 s ceiling. It completes on its own:
**345 s, 13/13 PASS** standalone on Windows, and **147 s, 13/13 PASS** on Linux.
This class was already known to be genuinely slow rather than stuck (it has a
1100 s entry in the runner's `$slowClasses` table); the 1500 s result came from
a `-Parallel 2` shard, i.e. contention on top of an already-slow class. No VM
defect found.

---

## 7. `EmbeddedLdapAutoConfigurationTests` — Windows-only legacy-DSA TLS gap (still open)

1 of 17 fails, `whenSslBundleIsConfiguredLdapsListenerIsConfigured`, and only on
Windows — the class passes 17/17 on Linux.

```
IOException(ServerConfig with_single_cert failed: unexpected error: failed to parse private key
as RSA, ECDSA, or EdDSA; platform TLS fallback: <CRYPT_E_ASN1_BADTAG, os error -2146881269>)
```

The fixture keystore
(`module/spring-boot-ldap/src/test/resources/.../test.jks`) holds a **1024-bit
DSA key** with a `SHA1withDSA` self-signed certificate (confirmed with
`keytool -list -v`). rustls has no DSA `SigningKey` at all, and its TLS 1.2
cipher suites are all `ECDHE_RSA` / `ECDHE_ECDSA` — a DSA certificate needs a
`DHE_DSS` suite, which rustls does not implement. `t27_tls.rs` therefore falls
back per platform:

* `#[cfg(unix)]` → `legacy_dsa_acceptor`, an OpenSSL acceptor with
  `set_security_level(0)`, which handles DSA and other legacy identities;
* `#[cfg(not(unix))]` → `native_tls::Identity::from_pkcs8` → Windows SChannel,
  which rejects the DSA key outright, and whose modern builds no longer offer
  `TLS_DHE_DSS_*` suites even if the import succeeded.

So this is a genuine Unix/Windows platform-parity gap, not a newly-hit rustls
limitation. Real HotSpot passes because JSSE still implements `DHE_DSS`.

Closing the gap means putting OpenSSL into the Windows dependency graph, which
a feasibility build showed would require a full Perl distribution on every
Windows build machine. **Decision (2026-07-31): accepted as a documented
Windows-only limitation** rather than taking that toolchain cost. The full
analysis, the ruled-out alternatives, and the exact code change should it ever
be revisited are in
`docs/known-issues/springboot-ldap-dsa-tls-windows-only-gap.md`.
