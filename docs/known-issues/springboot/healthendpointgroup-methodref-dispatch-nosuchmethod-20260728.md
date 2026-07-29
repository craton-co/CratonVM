# `HealthEndpointGroup::getAdditionalPath` method-reference lambda dispatches against erased `Object`, not the real target type

**Status: OPEN — found 2026-07-28**

## Symptom

| Module | Class | Test method |
|---|---|---|
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.servlet.ManagementWebSecurityAutoConfigurationTests` | `withAdditionalPathsOnSamePort` |

```
=> java.lang.NoSuchMethodError: java.lang.Object.getAdditionalPath()Lorg/springframework/boot/health/actuate/endpoint/AdditionalHealthEndpointPath;
       org.springframework.boot.health.autoconfigure.actuate.endpoint.AutoConfiguredHealthEndpointGroups.getAdditionalPaths(AutoConfiguredHealthEndpointGroups.java:172)
       org.springframework.boot.actuate.endpoint.web.annotation.DiscoveredWebEndpoint.getAdditionalPaths(DiscoveredWebEndpoint.java:64)
       org.springframework.boot.actuate.endpoint.web.annotation.DiscoveredWebEndpoint.lambda$getAdditionalPaths$0(DiscoveredWebEndpoint.java:59)
       org.springframework.boot.actuate.endpoint.web.annotation.DiscoveredWebEndpoint.getAdditionalPaths(DiscoveredWebEndpoint.java:60)
       org.springframework.boot.actuate.endpoint.web.PathMappedEndpoints.getAdditionalPaths(PathMappedEndpoints.java:137)
       org.springframework.boot.security.autoconfigure.actuate.web.servlet.EndpointRequest$AdditionalPathsEndpointRequestMatcher.streamAdditionalPaths(EndpointRequest.java:472)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260728/shard2/logs/module_spring-boot-security.org.springframework.boot.security.autoconfigure.actuate.web.ser-f4cd03f47d82.out.log`

Thrown mid-request, while a `RequestMatcher` (`EndpointRequest`'s
`AdditionalPathsEndpointRequestMatcher`) evaluates whether an incoming
request path matches an actuator health-endpoint additional path — during
the security filter chain's authorization check.

## Root cause (confirmed via source reading)

`AutoConfiguredHealthEndpointGroups.getAdditionalPaths`
(`apps/spring-boot/module/spring-boot-health/src/main/java/org/springframework/boot/health/autoconfigure/actuate/endpoint/AutoConfiguredHealthEndpointGroups.java:163-172`):

```java
public @Nullable List<String> getAdditionalPaths(EndpointId endpointId, WebServerNamespace webServerNamespace) {
    if (!HealthEndpoint.ID.equals(endpointId)) {
        return null;
    }
    return streamAllGroups().map(HealthEndpointGroup::getAdditionalPath)
        .filter(Objects::nonNull)
        .filter((additionalPath) -> additionalPath.hasNamespace(webServerNamespace))
        .map(AdditionalHealthEndpointPath::getValue)
        .toList();
}
```

`.map(HealthEndpointGroup::getAdditionalPath)` is an unbound
instance-method reference used as a `Function<HealthEndpointGroup,
AdditionalHealthEndpointPath>` for `Stream.map`. The `LambdaMetafactory`-
generated bridge for this shape must, for each stream element, cast the
`Object`-typed SAM parameter (`Function.apply(Object):Object`) to the
method reference's real receiver type (`HealthEndpointGroup`) before
invoking `getAdditionalPath()` on it — this is exactly what makes an
unbound instance-method-reference lambda type-safe.

The observed `NoSuchMethodError: java.lang.Object.getAdditionalPath()...`
means that cast/retarget step did not happen: the invoke was resolved
directly against `java.lang.Object` (the SAM parameter's *erased* static
type), which has no `getAdditionalPath()` method at all. This is a
dispatch-generation bug for this exact lambda shape (unbound instance
method reference on a stream), not a data problem — the stream's elements
are genuinely `HealthEndpointGroup` instances (`streamAllGroups()` returns
`Stream<HealthEndpointGroup>`).

This matches a well-established bug *family* in this codebase's JIT/indy
lambda-dispatch layer — prior instances (all now fixed) include wrong-
receiver `invokevirtual`/`invokespecial` dispatch for lambdas
(`wrong-receiver-virtual-dispatch-corruption-cluster-FIXED.md`), self-call
rerouting (`reference_jit_selfcall_dispatch_reroute.md`), and
descriptor-coercion/slot-reuse traps
(`reference_descriptor_coercion_slot_reuse_trap.md`) — but this specific
call shape (a bare, unbound `Type::instanceMethod` reference passed
directly to `Stream.map`, invoked from inside
`AutoConfiguredHealthEndpointGroups`) was not found in any existing doc in
this codebase and is not confirmed to share the exact same code-level cause
as any of those prior fixes.

## Confirming/refuting this hypothesis

Reproduce standalone: a `Stream<X>.map(X::someMethod)` where `X` is a
concrete class (not an interface/generic-erased type), run under CratonVM
with `CRATONVM_DBG_JIT_DISASM` or an indy-focused trace, to see whether the
generated call site casts to `X` before invoking, or invokes directly
against the SAM's erased parameter type. If it reproduces standalone, this
narrows to CratonVM's `invokedynamic`/`LambdaMetafactory` bootstrap for
unbound instance-method references specifically (as opposed to bound
references or static-method references, which may go through a different
code path and could be unaffected — not tested here).

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-security` | `org.springframework.boot.security.autoconfigure.actuate.web.servlet.ManagementWebSecurityAutoConfigurationTests` |

## Note on a prior doc covering the same class

`docs/internal/fixed-suite-bugs/springboot/onbeancondition-mergedannotations-intermittent-identity-mismatch-FIXED.md`
also lists `ManagementWebSecurityAutoConfigurationTests` as affected, but
for a **completely different, already-closed** symptom
(`MergedAnnotations`/`@ConditionalOnMissingBean` identity mismatch — that
doc's own text says it "never reproduced against current `dev`" and was
closed 2026-07-27). This 2026-07-28 failure is a **different test method**
(`withAdditionalPathsOnSamePort`) with a **different exception type**
(`NoSuchMethodError`, not the `IllegalStateException` that doc tracked) — a
new, unrelated finding for the same class, not a regression of that closed
doc.
