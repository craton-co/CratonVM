# Jetty factory post-startup timeout and reflective-supertype residuals

**Status: OPEN - confirmed 2026-07-18**

## Scope and separation

This is deliberately separate from the fixed private-lambda owner-dispatch
issue. The former `StackOverflowError`, duplicate-registration, and
`FilterRegistration.Dynamic` symptoms are absent. The remaining failures occur
after normal Jetty startup or in `Method.invoke` assignability validation.

## Reproduction

Using Spring Boot 4.1.0-SNAPSHOT with Jetty 12.1.8 and the direct `SbRunner`
launcher, HotSpot/JDK 25 passes all three affected classes:

| Class | HotSpot | CratonVM JIT | CratonVM `--nojit` |
|---|---:|---:|---:|
| `JettyReactiveWebServerFactoryTests` | 35 pass, 1 skipped | timeout after 180s | timeout after 180s |
| `JettyServletWebServerFactoryTests` | 113 pass, 2 skipped | timeout after 180s | timeout after 180s |
| `JettyServletWebServerServletContextListenerTests` | 2 pass | 2 pass | 2 pass |

The two factory logs show repeated successful `ServletContextHandler` and
`Server` startups before the timeout, not recursion or duplicate servlet
registration. The listener failure is:

```
IllegalArgumentException: object of type
org.springframework.boot.jetty.autoconfigure.servlet.JettyServletWebServerServletContextListenerTests
is not an instance of
org.springframework.boot.web.server.servlet.AbstractServletWebServerServletContextListenerTests
```

A 30-second `--stack-dump-on-timeout` capture of the reactive factory class
places the main thread in
`AbstractReactiveWebServerFactoryTests.compressionOfResponseToGetRequest` ->
`Mono.block(Duration)` -> `BlockingSingleSubscriber.blockingGet`. Jetty worker
threads are idle in `QueuedThreadPool` wait sites. This rules out continued
`startContext()` recursion and narrows the timeout to post-startup request /
response delivery.

## Fixed reflective-supertype residual

`loader_aware_reflect_assignable` now walks the receiver's resolved superclass
chain before rejecting a class target. This preserves a valid relation when a
reflective `Method` mirror holds a different loader copy of a superclass. The
listener class passes 2/2 in JIT and `--nojit` with the correction.

## Remaining direction

The two factory timeouts reproduce with and without JIT. They must be traced
through their post-startup request/response path before changing dispatch or
timeout policy.
