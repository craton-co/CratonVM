# `spring-boot-r2dbc`: filtered `loadClass` override honored

**Status: RESOLVED — 2026-07-18.**

`EmbeddedDatabaseConnectionTests` and
`ConnectionFactoryBeanCreationFailureAnalyzerTests` use direct
`URLClassLoader` subclasses to hide R2DBC provider classes.

## Resolution and validation

The dedicated native regression test now covers that exact hierarchy and
verifies that base `loadClass(String)` dispatch detects and invokes the
protected `loadClass(String, boolean)` override. The two real R2DBC classes
(11 tests) passed with JIT enabled and with `--nojit` on 2026-07-18.
