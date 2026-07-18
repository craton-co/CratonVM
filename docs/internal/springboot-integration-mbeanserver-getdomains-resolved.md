# Spring Boot Integration MBeanServer domains and scheduler residuals — resolved

Resolved 2026-07-18.

`IntegrationAutoConfigurationTests` previously failed because the synthetic
`MBeanServer` lacked `getDomains()`, `ObjectName.getInstance(String)` accepted
invalid bare bean names instead of throwing `MalformedObjectNameException`,
primitive lambda unboxing preserved a compact `int` where a `long` was
required, and real-JDK scheduled-executor bridges exposed stale synthetic slot
values.

The fix registers `MBeanServer.getDomains()` from the live MBean registry,
enforces the required ObjectName structure so Spring's package-domain fallback
runs, widens lambda unboxing to the requested primitive type, and keeps only
real-layout-safe scheduled-executor constructor/getter bridges in real-JDK
mode. Reflection reads now see the actual `corePoolSize` field.

Validation used the exact Spring Boot fixture class
`org.springframework.boot.integration.autoconfigure.IntegrationAutoConfigurationTests`:

- JIT enabled: 34/34 passed.
- JIT disabled: 34/34 passed.

The earlier baseline had five failures in this class: two JMX domain failures,
two corrupted poller `long` values, and the scheduler pool-size residual.
