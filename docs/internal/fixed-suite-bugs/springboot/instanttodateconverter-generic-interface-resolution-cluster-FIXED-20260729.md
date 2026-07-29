# InstantToDateConverter generic-interface resolution cluster — FIXED 2026-07-29

## Closure

`InstantToDateConverter` does implement both `ConditionalConverter` and
`Converter<Instant, Date>`, but the original report's suspected
`typesig_to_real_type` fallback was not reached. The generic arguments were
lost only when a JIT-compiled caller crossed the forced native bridge for
`Class.getGenericInterfaces()` / `getGenericSuperclass()` /
`getTypeParameters()` and ran the real-JDK bytecode instead of the VM's
Signature-attribute implementation.

The same workload also uncovered a separate JIT-only watchdog: virtual
tier-up caches use a symbolic owner that can be an interface rather than the
concrete `java.util` receiver. A hot metadata collection receiver could then
publish an unsuitable virtual entry and spin. That residual is now excluded
from *virtual* tier-up using the validated receiver ClassId; direct and static
JIT compilation remain eligible.

## Fix

- Treat forced real-JDK native overrides as native shadows while resolving JIT
  direct-call targets.
- Refuse background callee compilation for callers of the three forced Class
  generic-metadata bridges, preserving interpreter dispatch to the native
  Signature reifier.
- Exclude concrete `java/util/*` receivers from the virtual tier-up cache
  path only.

## Validation

Verified with `apps/spring-boot-suite-runner` against the complete local
Spring Boot fixture at
`C:\\craton\\CratonVM-spring-boot-rerun-20260717\\apps\\spring-boot`.
The requested `C:\\craton\\CratonVM\\apps\\spring-boot` checkout was not a
valid runner fixture: required generated runtime classpaths and Boot jars were
missing, and HotSpot failed before test discovery with
`NoClassDefFoundError: org.springframework.boot.context.annotation.Configurations`.

Final release binary: SHA-256
`161B151AC995A34FAD9F1D0E647A52F713D87E80413C02BA9414416DFC12523F`.

| VM/mode | Jedis tests | Health tests | Result |
|---|---:|---:|---|
| CratonVM JIT | 23/23 | 2/2 | PASS |
| CratonVM `--nojit` | 23/23 | 2/2 | PASS |
| HotSpot | 23/23 | 2/2 | PASS |

The CratonVM JIT run completed in 176.9 s; no-JIT completed in 141.4 s.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-data-redis` | `org.springframework.boot.data.redis.autoconfigure.DataRedisAutoConfigurationJedisTests` |
| `module/spring-boot-data-redis` | `org.springframework.boot.data.redis.autoconfigure.health.DataRedisHealthContributorAutoConfigurationTests` |
