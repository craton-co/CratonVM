# `java.net.http.HttpClient$Builder`: `proxy`/`sslContext`/`cookieHandler`/`authenticator`/`sslParameters` throw `AbstractMethodError` — dead registration, not reachable in real-JDK builds

**Status: RESOLVED (2026-07-12).**

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
The active real-JDK registrar, `native-builtins/src/net_phase_e.rs::register_re5_http_client`, created the one-field builder but registered only `build`, `connectTimeout`, and `followRedirects` from the interface. The larger `http2.rs` registration is synthetic-only and uses an incompatible eight-field builder layout, so it cannot safely be reused for this real-JDK object.

A complete Java 17 interface audit also found two residuals outside the original Spring stack traces: `version` had no real-JDK registration because its phase-60 fallback is likewise synthetic-only, and `priority(int)` was absent from both registrars. Every missing method resolved to the Code-less abstract declaration and therefore threw `AbstractMethodError`.

## Suggested fix

Register the complete Java 17 fluent surface directly in
`register_re5_http_client`, preserving the existing one-field layout and
stateless receiver-return behavior. Do not import the `http2.rs` closures,
which write the incompatible eight-field layout.

## Repro

```powershell
$env:JAVA_HOME = 'C:\Program Files\Java\jdk-25'
apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 -SpringBootRoot C:\craton\CratonVM\apps\spring-boot `
  -ClassList <a TSV with header 'module<TAB>class' and one row 'module/spring-boot-http-client<TAB>org.springframework.boot.http.client.JdkClientHttpRequestFactoryBuilderTests'> `
  -Start 1 -Count 1 -Exe <cratonvm exe>
```

## Resolution

The default real-JDK registrar is `net_phase_e::register_re5_http_client`, not
`http2::register_http_client_builder` or the synthetic-only phase-60 block.
It now registers every Java 17 `HttpClient.Builder` fluent interface method
against its canonical one-field synthetic layout: `version`, `priority`,
`connectTimeout`, `followRedirects`, `executor`, `cookieHandler`, `proxy`,
`authenticator`, `sslContext`, and `sslParameters`. Each returns the receiver,
matching the existing stateless real-JDK builder behavior; `build` remains
registered by the same path.

Validation covers the full registration set in
`re5_real_jdk_http_client_builder_fluent_methods_are_registered` and a real-JDK
Java 17 probe invokes every method before `build()`. This eliminates both the
13 observed Spring Boot failures and the previously unobserved `version` /
`priority` residuals from the same dead-registration family.
