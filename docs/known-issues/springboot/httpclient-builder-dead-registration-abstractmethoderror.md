# `java.net.http.HttpClient$Builder`: `proxy`/`sslContext`/`cookieHandler`/`authenticator`/`sslParameters` throw `AbstractMethodError` — dead registration, not reachable in real-JDK builds

**Status: OPEN, root-caused. Severity: HIGH (broad, deterministic).**

Found while investigating the `FAIL` cluster from the first full Spring Boot
suite run (see [[project_spring_boot_suite_runner_20260711]]). At least 13
test classes across 8 modules (`spring-boot-http-client`,
`spring-boot-micrometer-metrics`, `spring-boot-restclient`,
`spring-boot-resttestclient`, `spring-boot-webclient`, `spring-boot-webmvc`,
`spring-boot-webservices-test`) fail — `JdkClientHttpRequestFactoryBuilderTests`
alone loses 32/32 tests — with:

```
java.lang.AbstractMethodError: method java/net/http/HttpClient$Builder.proxy(Ljava/net/ProxySelector;)Ljava/net/http/HttpClient$Builder; has no Code attribute
    org.springframework.boot.context.properties.PropertyMapper$Source.to(PropertyMapper.java:292)
    org.springframework.boot.http.client.JdkHttpClientBuilder.build(JdkHttpClientBuilder.java:108)
```

Same shape for `.sslContext(...)`, `.cookieHandler(...)`, and (by the same
root cause, not yet directly observed in a log but certain given the
registration gap below) `.authenticator(...)` / `.sslParameters(...)`.

## Root cause

`HttpClient.newBuilder()` returns a bare synthetic object whose runtime class
*is* the interface `java/net/http/HttpClient$Builder` itself (via
`alloc_concurrent_synthetic`) — there is no concrete
`jdk.internal.net.http.HttpClientBuilderImpl` bytecode class backing it, so
**every** `Builder` method call depends entirely on a registered native
intercepting the interface's own (Code-less) abstract method declaration.
Two competing, non-identical registrations exist for this same class:

- `native-builtins/src/http2.rs::register_http_client_builder` — the
  **complete** set: `version`, `connectTimeout`, `followRedirects`,
  `executor`, `cookieHandler`, `proxy`, `authenticator`, `sslContext`,
  `sslParameters`, `build`.
- `native-builtins/src/net_phase_e.rs::register_re5_http_client` — a
  **partial** set: only `newBuilder` (static), `connectTimeout`,
  `followRedirects`.

`register_http_client_builder` is only ever reached via
`http2.rs::register_http2_natives`, which is called from
`lib.rs::register_synthetic_overrides` (`native-builtins/src/lib.rs:37897`,
`#[cfg(feature = "synthetic-jdk")]`) — itself only called from
`register_builtins`, ALSO `#[cfg(feature = "synthetic-jdk")]`-gated
(`lib.rs:37886`). **The default `cratonvm-cli` build does not enable the
`synthetic-jdk` feature**, so `register_http_client_builder`'s complete
method set is compiled out entirely and never registered. Only
`net_phase_e.rs`'s partial set (reached from the real-JDK path via
`register_phase_e_networking`, called near `lib.rs:33987`) survives —
explaining exactly the observed split: `version`/`connectTimeout`/
`followRedirects` work, `proxy`/`sslContext`/`cookieHandler`/`authenticator`/
`sslParameters` don't.

This is the same recurring **"synthetic-only registration missing in
essential"** pattern already hit multiple times in this codebase (see the
`register_p68_ssl`/`register_nio_natives` fixes referenced in
`docs/known-issues/README.md` and
[[reference_synthetic_jdk_dead_registration_trap]]) — a registration function
authored as the "complete, correct" implementation, gated behind
`synthetic-jdk`, silently absent from every real-JDK suite run including this
one.

## Suggested fix

Call the missing methods (`proxy`, `authenticator`, `cookieHandler`,
`sslContext`, `sslParameters`, `executor`) from a real-JDK-reachable path —
either extend `net_phase_e.rs::register_re5_http_client` directly, or add an
explicit `register_http_client_builder(registry)` call from the real-JDK
essential-natives path (mirroring how `register_p68_ssl` was pulled out of
`register_synthetic_overrides` into its own real-mode call). Verify field
indices match: `http2.rs`'s closures write to slots 0/1/2/4/5/6/7 assuming
its own 8-field synthetic layout (`alloc_concurrent_synthetic(ctx,
"java/net/http/HttpClient$Builder", 8)`), while `net_phase_e.rs` allocates
only 1 field for the same class name
(`alloc_concurrent_synthetic(ctx, "java/net/http/HttpClient$Builder", 1)`) —
these are two *different* synthetic layouts sharing a class name; reconcile
before wiring both sets of natives to the same object, or the newly-added
methods will silently write out of bounds.

## Repro

```powershell
$env:JAVA_HOME = 'C:\Program Files\Java\jdk-25'
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -ClassList <a TSV with header 'module<TAB>class' and one row 'module/spring-boot-http-client<TAB>org.springframework.boot.http.client.JdkClientHttpRequestFactoryBuilderTests'> `
  -Start 1 -Count 1 -Exe <cratonvm exe>
```
