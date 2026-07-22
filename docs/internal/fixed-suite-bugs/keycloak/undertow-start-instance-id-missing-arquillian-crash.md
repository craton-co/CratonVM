# Undertow `start()` instance-ID crash during Keycloak Arquillian bootstrap ? fixed

Status: fixed and retired on 2026-07-16.

## Root cause

CratonVM registered native Undertow lifecycle methods but allowed the concrete
real-JDK `Undertow`/`Undertow.Builder` bytecode to run on the same path. The
native `start()` then expected compact synthetic fields on a real-layout object.
In particular, the attempted numeric instance-ID write collided with Undertow's
reference-typed listener field and was discarded as `null`, causing:

```
internal error: Undertow.start: instance id missing
```

The real builder layout has the same incompatibility for listener, handler, and
thread-setting fields.

## Fix

- Force the registered native Undertow lifecycle and supported builder methods
over real bytecode in both normal and cached dispatch paths.
- Add the missing native `Builder.setSocketOption` bridge used by Keycloak.
- Store native Undertow instance IDs and all compact builder state in VM-stable
identity side tables rather than real-JDK instance slots.
- Pin the builder around allocation where native execution can trigger GC.

## Validation

The original Azure reproduction (`KcRegTest`) was a `CRASH` on current `dev`
with `Undertow.start: instance id missing`. With the repair it reaches normal
JUnit reporting in both interpreter and JIT modes; neither run contains the
Undertow instance-ID or malformed-listener crash. The next result is a distinct
Keycloak bootstrap NPE in
`AuthServerTestEnricher.setJsseSecurityProviderForOutboundSslConnectionsOfElytronClient`
while resolving security providers, after the Undertow container has started.
That is outside the native Undertow lifecycle contract closed here.

Focused native Undertow coverage: 23 passed, 0 failed.
