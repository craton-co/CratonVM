# `java.net.http.HttpClient`/`HttpClient$Builder`: configured options are dropped (field-slot collision + boolean-only storage + `send()` ignores them entirely)

**Status: OPEN — found 2026-07-17**

## Symptom

| Class | Method(s) | Failure |
|---|---|---|
| `module/spring-boot-http-client` `JdkClientHttpRequestFactoryBuilderTests` | `redirectDontFollow(String)` [5 params] | `AssertionFailedError: expected: 302 FOUND but was: 200 OK` |
| same | `filteredInetAddress()` | `AssertionError: Expecting code to raise a throwable.` |
| same | `cookieHandlingEnabled(HttpCookieHandling)` [2 params] | `AssertionError: Expecting Optional to contain a value but it was empty.` |
| same | `withProxySelectorWhenHasInetAddressMatcher()` | `NoSuchElementException: No value present` |
| same | `withProxySelector()` | `AssertionError: Expecting Optional to contain: Mock for ProxySelector ... but was empty.` |
| same | `withExecutor()` | `AssertionError: Expecting Optional to contain: SimpleAsyncTaskExecutor@... but was empty.` |
| same | `buildWhenHasConnectTimeout()` | `NoSuchElementException: No value present` |
| same | `connectWithSslBundle(String)` [2 params] | `AssertionError: expected SSLHandshakeException but was IOException: TLS handshake: ... (os error -2146762487)` — see "Bug 3 / SSL angle" below |
| same | `connectWithSslBundleAndOptionsMismatch(String)` [2 params] | same shape |
| `module/spring-boot-http-client` `reactive.JdkClientHttpConnectorBuilderTests` | `buildWhenHasConnectTimeout`, `connectWithSslBundleAndOptionsMismatch`, `redirectDontFollow`, `filteredInetAddress`, `connectWithSslBundle`, `withProxySelectorWhenHasInetAddressMatcher`, `withProxySelector`, `withExecutor` | same 14 failures, byte-identical shapes to the non-reactive class above (this connector shares the same underlying `java.net.http.HttpClient` implementation) |
| `module/spring-boot-http-client` `autoconfigure.imperative.ImperativeHttpClientAutoConfigurationTests` | `whenVirtualThreadsEnabledAndUsingJdkHttpClientUsesVirtualThreadExecutor()` | `AssertionError: Expecting Optional to contain a value but it was empty.` |
| `module/spring-boot-http-client` `autoconfigure.reactive.ReactiveHttpClientAutoConfigurationTests` | `whenVirtualThreadsEnabledAndUsingJdkHttpClientUsesVirtualThreadExecutor()` | same shape |

16/32 tests fail in `JdkClientHttpRequestFactoryBuilderTests` alone. Representative traces:

```
JUnit Jupiter:JdkClientHttpRequestFactoryBuilderTests:redirectDontFollow(String):[1] httpMethod = "GET"
    => org.opentest4j.AssertionFailedError:
expected: 302 FOUND
 but was: 200 OK
       org.springframework.boot.http.client.AbstractClientHttpRequestFactoryBuilderTests.testRedirect(AbstractClientHttpRequestFactoryBuilderTests.java:182)
       org.springframework.boot.http.client.AbstractClientHttpRequestFactoryBuilderTests.redirectDontFollow(AbstractClientHttpRequestFactoryBuilderTests.java:166)

JUnit Jupiter:JdkClientHttpRequestFactoryBuilderTests:withProxySelector()
    => java.lang.AssertionError:
Expecting Optional to contain:
  Mock for ProxySelector, hashCode: 507689
but was empty.
       org.springframework.boot.http.client.JdkClientHttpRequestFactoryBuilderTests.withProxySelector(JdkClientHttpRequestFactoryBuilderTests.java:83)

JUnit Jupiter:JdkClientHttpRequestFactoryBuilderTests:buildWhenHasConnectTimeout()
    => java.util.NoSuchElementException: No value present
       org.springframework.boot.http.client.JdkClientHttpRequestFactoryBuilderTests.connectTimeout(JdkClientHttpRequestFactoryBuilderTests.java:131)
```

Full logs:
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-http-client.org.springframework.boot.http.client.JdkClientHttpRequestFactoryBuilderTests.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-http-client.org.springframework.boot.http.client.reactive.JdkClientHttpConn-c9b77cf330ac.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-http-client.org.springframework.boot.http.client.autoconfigure.imperative.I-58f1291c123e.out.log`
- `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard3/logs/module_spring-boot-http-client.org.springframework.boot.http.client.autoconfigure.reactive.Rea-4a8014b6cdc0.out.log`

The `.err.log` files for all of these contain only the pre-existing, unrelated
`gen_heap::get_field: out-of-bounds field read dropped ... org/junit/jupiter/engine/execution/InterceptingExecutableInvoker`
noise (see `docs/internal/app-jvm-bugs/bug-wildfly-get-field-factory-noise.md`)
— not part of this cluster's causal chain.

## Root cause (CONFIRMED at file:line precision)

`java.net.http.HttpClient` and `HttpClient$Builder` are entirely native
synthetic objects in CratonVM — `native-builtins/src/http2.rs`,
`alloc_http_client`/`alloc_http_client_builder` (lines 562-590). There are
actually **four separate, competing native registrations** for
`java/net/http/HttpClient`/`HttpClient$Builder` in this codebase
(`net_phase_e.rs::register_re5_http_client`, `phases_late.rs::register_p60_http_client`,
`http2.rs::register_http_client`/`register_http_client_builder`, and
`http_client.rs::register_http_client_real` which targets a different,
`jdk/internal/net/http/*`, implementation layer). Per the established
last-registration-wins pattern documented elsewhere in this project (see
`docs/internal/fixed-suite-bugs/wrong-receiver-virtual-dispatch-corruption-cluster-FIXED.md`),
and confirmed by call-site ordering in `native-builtins/src/lib.rs`
(`register_phase_e_networking` at :34966 → `register_phase60_natives` at
:41433 → `register_http2_natives` at :41548, **last wins**), `http2.rs`'s
implementation is the live one for the public `java.net.http.HttpClient`
surface. It has three distinct, concrete bugs:

### Bug 1 — `followRedirects()`/`cookieHandler()` field-slot collision drops the redirect policy

`register_http_client_builder` (`native-builtins/src/http2.rs:1281`):

- `followRedirects()` (~line 1319-1336) writes the `Redirect` enum value to
  Builder slot **1**, then ALSO writes a `policy != NEVER` boolean to Builder
  slot **7**.
- `cookieHandler()` (~line 1356-1368) writes its own "has a cookie handler"
  boolean to Builder slot **7** — **the same slot**.
- `Builder.build()` (~line 1437-1456) copies Builder's 8 slots (`for i in
  0..8usize`) into the client, mapping slot 7 → `CLIENT_HAS_COOKIE` (line
  1449). It never copies anything into the client's dedicated
  `CLIENT_FOLLOW_REDIR` slot (constant `= 8`, `http2.rs:525`) — that slot only
  exists on the 10-field client object (`alloc_http_client`, line 563), not
  on the 8-field Builder, so nothing the Builder ever wrote can reach it. The
  client's `CLIENT_FOLLOW_REDIR` stays permanently at its `alloc_http_client`
  default of `0` (line 572).
- Whichever of `followRedirects()`/`cookieHandler()` runs last on a given
  Builder instance wins slot 7, silently corrupting the other's flag.

### Bug 2 — `executor()`/`cookieHandler()`/`proxy()`/`authenticator()` only store a presence boolean, never the real object

Each of these Builder methods (`http2.rs:1341-1420`) does the same thing:

```rust
let has = match args.get(1) {
    Some(Value::Object(Some(_))) => 1,
    _ => 0,
};
ctx.set_field(this, N, Value::Int(has));
```

The actual configured `Executor`/`CookieHandler`/`ProxySelector`/
`Authenticator`/`SSLContext` object reference is discarded — only a 0/1
"was something passed" flag survives. The corresponding accessor methods
(`HttpClient.executor()`, `.cookieHandler()`, `.proxy()`, `.authenticator()`,
lines ~1175-1230) allocate a **1-field** synthetic `java/util/Optional` and
write that same boolean into its single field (e.g. `executor()`:
`let opt = alloc_concurrent_synthetic(ctx, "java/util/Optional", 1); ctx.set_field(opt, 0, Value::Int(has));`).
`java.util.Optional` is a real, pure-Java JDK class with **one** field
(`private final Object value`; presence is `value != null`, not a separate
flag) — so this 1-field synthetic, with an `Int` in the slot real
`Optional.isPresent()`/`.get()` bytecode expects to hold the actual value
object (or `null`), does not round-trip through real `Optional` bytecode as
"present with value 1". Because of the type mismatch, real `Optional.get()`
sees no usable value and throws `NoSuchElementException: No value present`
(exactly the observed symptom) or AssertJ's `assertThat(optional).contains(x)`
correctly reports it as empty. This explains `withProxySelector`,
`withProxySelectorWhenHasInetAddressMatcher`, `withExecutor`, and
`cookieHandlingEnabled` uniformly, and (via `ImperativeHttpClientAutoConfigurationTests`
/`ReactiveHttpClientAutoConfigurationTests` reflectively checking the
configured virtual-thread `Executor`) the two autoconfig failures too.

`connectTimeout()` (`HttpClient`, ~line 1142-1158) is slightly better — it
allocates a **2**-field synthetic Optional and puts the real `Long` value in
slot 1 — but it is still a synthetic object under the real `java/util/Optional`
class name with a layout (`[presence:Int, value:Long]`) that doesn't match
real `Optional`'s single `value` field either, so `buildWhenHasConnectTimeout()`
/the reactive connector's equivalent test still hits
`NoSuchElementException: No value present` when real `Optional.get()`
bytecode reads field slot 0 expecting the actual `Duration` value.

### Bug 3 — `HttpClient.send()` never consults any of the stored configuration at all

`register_http_client`'s `send()` implementation (`http2.rs:938-1040`)
extracts only the method, URI, host/port/path from the `HttpRequest` and
calls `https_request(&host, port, method_str, &path)` /
`http11_request(&host, port, method_str, &path)` directly. It never reads
`CLIENT_REDIRECT`/`CLIENT_FOLLOW_REDIR`, `CLIENT_HAS_PROXY`, or any other
client-configuration field at all. So regardless of the storage bugs above,
**no builder-configured behavior (redirect policy, InetAddress/proxy
filtering, custom trust store) has any effect on an actual request** — the
underlying `https_request`/`http11_request` transport's own hardcoded
default behavior (which evidently follows redirects unconditionally) always
applies. This is the direct cause of `redirectDontFollow` (expects the
factory to surface a raw 302, gets a followed-and-resolved 200) and
`filteredInetAddress` (expects connecting to a filtered/disallowed address to
throw, but nothing ever consults the filter/proxy selector that would
enforce it).

This same mechanism (`sslContext()` also only stores a presence boolean, per
Bug 2) is very likely why `JdkClientHttpRequestFactoryBuilderTests.connectWithSslBundle`/
`connectWithSslBundleAndOptionsMismatch` also fail with a TLS
trust-chain error (`os error -2146762487` = Windows `CERT_E_UNTRUSTEDROOT`)
instead of using the SSLBundle's configured trust store — `send()` has no way
to reach the real `SSLContext` object even if it wanted to, so
`http_client.rs`'s `rustls::ClientConnection`-based TLS layer (used for the
actual handshake — see its module doc comment, "TLS uses `rustls::ClientConnection`
directly") always falls back to whatever default/global trust configuration
it has, rejecting the test's self-signed certificate. This SSL angle is
**not separately filed** here since it's the same "config discarded at the
Builder" root cause as Bugs 1-3, just observed through a different builder
option; it is a distinct *mechanism* from the SSLEngine/SSLSocket-based TLS
failures in non-JDK-HttpClient backends, documented separately in
`tls-sslbundle-trust-validation-gap-cluster.md`.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-http-client` | `org.springframework.boot.http.client.JdkClientHttpRequestFactoryBuilderTests` |
| `module/spring-boot-http-client` | `org.springframework.boot.http.client.reactive.JdkClientHttpConnectorBuilderTests` |
| `module/spring-boot-http-client` | `org.springframework.boot.http.client.autoconfigure.imperative.ImperativeHttpClientAutoConfigurationTests` (1 failure — the classpath-presence cluster is fixed; see `../../internal/springboot/httpclient-autoconfigure-classpath-presence-cluster-FIXED.md`) |
| `module/spring-boot-http-client` | `org.springframework.boot.http.client.autoconfigure.reactive.ReactiveHttpClientAutoConfigurationTests` (1 failure — the classpath-presence cluster is fixed; see `../../internal/springboot/httpclient-autoconfigure-classpath-presence-cluster-FIXED.md`) |
| `module/spring-boot-resttestclient` | `org.springframework.boot.resttestclient.TestRestTemplateTests` (3/38 failures — see Cross-reference below) |

## Cross-reference (2026-07-17, separate triage batch, same rerun)

`module/spring-boot-resttestclient`'s `TestRestTemplateTests` independently
fails 3 tests with the same shape, this time via `RestTemplateBuilder`
+ `ClientHttpRequestFactoryBuilder.jdk()` rather than the
`spring-boot-http-client` module's own builder tests directly:

```
JUnit Jupiter:TestRestTemplateTests:withClientSettingsRedirectsForJdk()
    => org.opentest4j.AssertionFailedError:
expected: NORMAL
 but was: NEVER
       org.springframework.boot.resttestclient.TestRestTemplateTests.withClientSettingsRedirectsForJdk(TestRestTemplateTests.java:216)
```
(and the identically-shaped `jdkBuilderCanBeSpecifiedWithSpecificRedirects`,
`withClientSettingsUpdateRedirectsForJdk`). Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard5/logs/module_spring-boot-resttestclient.org.springframework.boot.resttestclient.TestRestTemplateTests.out.log`.
This is the same Bug 1 (`followRedirects()`/`cookieHandler()` field-slot 7
collision, `Builder.build()` never populating `CLIENT_FOLLOW_REDIR`) from a
third consuming module — even the *default* (no explicit `.redirects(...)`
call) `HttpClient` built by Spring's JDK request-factory builder reads back
`Redirect.NEVER` instead of the `NORMAL` Spring configures as its baseline,
confirming the redirect policy is unconditionally lost regardless of which
Spring Boot API constructs the underlying `java.net.http.HttpClient`.
