# Quarkus: `OidcCommonConfig.authServerUrl()` `AbstractMethodError: method has no Code attribute`

## Status
**OPEN, root-caused.** Discovered during Quarkus test suite failures triage (`run-20260914-052033-passed`).

## Symptom
`OidcClientConfigBuilderTest` and related Quarkus OIDC/security tests fail with `AbstractMethodError`:

```
java.lang.AbstractMethodError: Method 'java.util.Optional io.quarkus.oidc.common.runtime.config.OidcCommonConfig.authServerUrl()' has no Code attribute
	at io.quarkus.oidc.common.runtime.config.OidcCommonConfigBuilder.build(OidcCommonConfigBuilder.java:42)
```

## Root Cause
`OidcCommonConfig` is an interface or interface with SmallRye Config / MicroProfile Config default annotations or default methods.

Under CratonVM:
1. Interface default method resolution or dynamic proxy/generated config implementation class generation (via SmallRye Config / Gizmo) misclassifies or omits the bytecode `Code` attribute during runtime `defineClass` / `defineHiddenClass`.
2. When the virtual/interface method `authServerUrl()` is invoked on the generated configuration proxy/instance, CratonVM's interpreter detects that the resolved method has no bytecode `Code` attribute and throws `AbstractMethodError`.

## Affected Tests / Scenarios
- `io.quarkus.oidc.client.runtime.OidcClientConfigBuilderTest`
- Quarkus OIDC and security configuration initialization tests.

## Remediation / Solution Plan
1. Inspect runtime class definition (`NativeContext::define_class_full` in `classloading` and `vm`) when parsing synthetic/generated interface proxy classes without explicit method implementations.
2. Fix method resolution to correctly dispatch interface default methods or generated proxy delegates rather than invoking un-implemented abstract slots.
