# Spring Boot Mockito inline real-method StackOverflowError cluster

**Status: FIXED - 2026-07-15**

## Closure

The original report attributed three shallow `StackOverflowError` traces to
the JIT dispatch-depth guard. Focused reruns disproved that theory: the
failure also reproduced with JIT disabled and the diagnostic stack contained a
repeating Mockito inline `CallsRealMethods` cycle.

The actual defect was CratonVM's native implementation of Mockito
`MockMethodAdvice.isOverridden`. It always treated inline mocks as not
overridden, so Mockito re-entered a superclass method through its real-method
adapter indefinitely. The initial direct-class check repaired the first two
classes. `RestTemplateBuilderTests` revealed the remaining inheritance case:
`RestTemplate` inherits `setRequestFactory` from
`InterceptingHttpAccessor`, while Mockito supplies the method declared by
`HttpAccessor`.

The native bridge now walks the concrete superclass chain and reports an
override only before it reaches Mockito's reflected declaring class. This
preserves normal interception of the declaring implementation while correctly
selecting the real inherited override.

As a related JIT correctness hardening, non-tail raw same-name static call
sites retain dispatch metadata rather than using a loader-identity-free direct
entry call.

## Verification

Fresh optimized CratonVM binary:

`target-jit-depth-closure-20260715-001/release/cratonvm-jitdepth-mockito-hierarchy-20260715-003.exe`

Final JIT-on run:

`../../../apps/spring-boot-suite-runner` result
`jitdepth-final-hierarchy-20260715-001/all-jit`

- `EnableConfigurationPropertiesRegistrarTests`: PASS, 6 tests, 58.3s.
- `LogbackLoggingSystemPropertiesTests`: PASS, 5 tests, 7.6s.
- `RestTemplateBuilderTests`: PASS, 52 tests, 32.9s.

Focused checks also passed:

- `cargo test -p cratonvm-jit self_recursive_nontail_site_routes_through_dispatch --lib`
- `cargo test -p cratonvm-native-builtins mockito_debugging_intrinsics_are_registered_for_location_factory --lib`

## Historical symptom

The affected classes previously failed with shallow-looking
`StackOverflowError` traces in ordinary Spring Boot calls. The shallow trace
was misleading because the actual recursion was inside Mockito's inline
real-method adapter, not a JIT depth guard overflow.
