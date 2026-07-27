# `com.sun.net.httpserver.HttpServer.setExecutor()` throws `AbstractMethodError: ... has no Code attribute` — CratonVM's `HttpServer.create()` returns an instance that doesn't properly override the abstract method

Status: open — genuine CratonVM-specific bug candidate

Date observed: 2026-07-14 (4-shard `nonpassed-v3` rerun against `others.tsv`, branch fix/keycloak-nonpassed-rerun-v2-20260710, binary `cratonvm-nonpassed-v3-refresh-20260714.exe`)

## Summary

3 classes across 2 modules (`adapters/saml/core`, `services`) fail identically in a shared `startHttpServer()`
test-helper pattern:

```
=> java.lang.AbstractMethodError: method com/sun/net/httpserver/HttpServer.setExecutor(Ljava/util/concurrent/Executor;)V has no Code attribute
   org.keycloak.adapters.saml.rotation.SamlDescriptorPublicKeyLocatorTest.startHttpServer(SamlDescriptorPublicKeyLocatorTest.java:95)
```

```
=> java.lang.AbstractMethodError: method com/sun/net/httpserver/HttpServer.setExecutor(Ljava/util/concurrent/Executor;)V has no Code attribute
   org.keycloak.connections.httpclient.DefaultHttpClientFactoryTest.startHttpServer(DefaultHttpClientFactoryTest.java:93)
```

```
=> java.lang.AbstractMethodError: method com/sun/net/httpserver/HttpServer.setExecutor(Ljava/util/concurrent/Executor;)V has no Code attribute
   org.keycloak.protocol.saml.profile.util.SoapTest.startHttpServer(SoapTest.java:93)
```

Identical exception, identical target method, across three unrelated test classes in two different modules —
all failing on the very first call after `HttpServer.create(...)` in their local `startHttpServer()` helper.

## Root cause hypothesis

`com.sun.net.httpserver.HttpServer` is an **abstract class**; `setExecutor(Executor)` is declared abstract on it
and is only ever meant to be invoked on a concrete subclass instance returned by the `HttpServer.create(...)`
factory (which goes through the `com.sun.net.httpserver.spi.HttpServerProvider` SPI to instantiate the JDK's real
concrete implementation, `sun.net.httpserver.ServerImpl`-backed `HttpServerImpl`). `AbstractMethodError: ... has
no Code attribute` is what the JVM throws when a virtual/interface dispatch resolves to the abstract method
declaration itself instead of a concrete override — i.e. the object CratonVM's `HttpServer.create(...)` hands
back is, from the dispatch machinery's point of view, still typed/vtable-resolved as the abstract `HttpServer`
class rather than the concrete provider implementation.

This points at CratonVM's handling of the `HttpServerProvider` SPI lookup or of the concrete `HttpServerImpl`
class's method table — either the returned object's class doesn't correctly register `setExecutor` as an
override (a class-layout/vtable-construction gap for this specific JDK internal class), or `HttpServer.create()`
itself returns an object whose runtime class is literally still `HttpServer` (the abstract class) rather than
the real provider's concrete subclass.

## Next steps

1. Trace `HttpServer.create(InetSocketAddress, int)` under CratonVM: confirm what concrete class the returned
   object actually reports via `getClass()` — if it prints `com.sun.net.httpserver.HttpServer` (the abstract
   class itself, not `sun.net.httpserver.HttpServerImpl` or similar), that directly confirms the provider-lookup
   theory.
2. Check whether `com.sun.net.httpserver.HttpServer`/`spi.HttpServerProvider` gets any special-cased native
   registration or synthetic-class treatment elsewhere in CratonVM (similar to other JDK SPI-based factories that
   have needed dedicated handling in this codebase) — this smells like a missing or incomplete registration for
   this specific JDK built-in HTTP server rather than a generic vtable bug (since `HttpServer` is a niche,
   rarely-exercised JDK class most test suites don't touch, unlike e.g. `Process`/`Spliterator` which get
   exercised constantly).
3. Once root-caused, re-verify against all 3 known classes plus a broader `com.sun.net.httpserver.*` sweep in
   case other methods (`start()`, `stop()`, `createContext()`) share the same underlying gap.

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-httpserver-setexecutor -ClassList <(printf 'module\tclass\nservices\torg.keycloak.protocol.saml.profile.util.SoapTest\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-v3-refresh-20260714.exe -JdkHome $jdk
```

Or a minimal standalone probe (no Keycloak involved):
```java
com.sun.net.httpserver.HttpServer server = com.sun.net.httpserver.HttpServer.create(new java.net.InetSocketAddress(0), 0);
System.out.println(server.getClass());
server.setExecutor(null);  // expect AbstractMethodError under CratonVM if this bug is confirmed
```

## Evidence

- `apps/keycloak-suite-runner/.suite/results/nonpassed-v3-shard1/all-jit/logs/adapters_saml_core.org.keycloak.adapters.saml.rotation.SamlDescriptorPublicKeyLocatorTest.out.log`
- `apps/keycloak-suite-runner/.suite/results/nonpassed-v3-shard1/all-jit/logs/services.org.keycloak.connections.httpclient.DefaultHttpClientFactoryTest.out.log`
- `apps/keycloak-suite-runner/.suite/results/nonpassed-v3-shard1/all-jit/logs/services.org.keycloak.protocol.saml.profile.util.SoapTest.out.log`

2026-07-14 rerun with binary `cratonvm-nonpassed-v3-refresh-20260714.exe` built from `dev` at commit `e85f76d00`.
