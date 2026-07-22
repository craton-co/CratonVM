# `spring-boot-jdbc`: hiding class-loader overrides honored

**Status: RESOLVED — 2026-07-18.**

Spring Boot JDBC tests use `URLClassLoader` subclasses that override protected
`loadClass(String, boolean)` to hide drivers and connection pools. CratonVM
had not carried direct-subclass coverage for this dispatch shape.

## Resolution and validation

The native class-loader regression test now exercises the direct
`URLClassLoader` subclass form, verifying rerouting from the base
`loadClass(String)` path to the protected override. The focused suite passed
`DataSourceBuilderTests`, `DataSourcePropertiesTests`, and
`DataSourceAutoConfigurationTests` (80 tests) in JIT and `--nojit` modes on
2026-07-18. The separately described empty-user-provided-data-source behavior
was not part of this class-loader cluster.
