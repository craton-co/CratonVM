# FIXED: `HttpServer.setExecutor()` no longer throws `AbstractMethodError`

Status: FIXED 2026-07-15 — validated on the Azure Linux host.

## Root cause

The native HTTP-server factory allocated the abstract public
`com.sun.net.httpserver.HttpServer` class. Its native surface omitted the
abstract `setExecutor(Executor)` and `getExecutor()` methods, so virtual
dispatch reached the abstract declaration and reported that it had no Code
attribute.

## Resolution

`HttpServer.create(...)` now returns the JDK's concrete
`sun.net.httpserver.HttpServerImpl` receiver. The native bridge surface is
aliased to that concrete class, including `setExecutor` and `getExecutor`.
The supplied executor is retained for the public getter contract, and changing
it after `start()` raises `IllegalStateException`, as required by the JDK API.

## Verification

On Azure, a fresh uniquely named release binary ran a focused real-JDK probe
that verified all of the following:

- `getClass().getName()` is `sun.net.httpserver.HttpServerImpl`.
- `setExecutor` and `getExecutor` retain an explicitly supplied executor.
- a bound server starts, reports its address, and stops cleanly.
- `setExecutor` after `start()` is rejected with `IllegalStateException`.

The three Keycloak residuals all use the same `setExecutor(null)` call in
their `startHttpServer()` setup and are covered by that contract:

- `SamlDescriptorPublicKeyLocatorTest`
- `DefaultHttpClientFactoryTest`
- `SoapTest`

The associated native-registry regression test verifies the executor bridges
on both the public API and the concrete receiver.
