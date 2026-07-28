# Spring Boot JTA, TestDatabase, and cold Mockito fixture residuals -- fixed 2026-07-28

## Closure

The original fresh-process Mockito `MockMethodAdvice` failures are no longer
present on the current `dev` baseline: the previously delivered loader and
reflection fixes cover the cold Mockito initialization path. A current
reproduction exposed one separate residual in
`JtaAutoConfigurationTests`: JDK JNDI provider discovery was being bypassed.

Real-JDK runs registered CratonVM's synthetic `javax.naming.InitialContext`
constructor and operation shims. Those shims do not execute the JDK
`InitialContext` provider-selection bytecode, so the test's
`jndi.properties` was ignored and `new InitialContext()` threw
`NoInitialContextException`. The synthetic naming registrations are now
limited to `synthetic-jdk` builds; real-JDK runs use the JDK implementation,
which discovers the application resource and its `SimpleJndiContextFactory`.

## Validation

The release binary was built from this change and every class was run in its
own fresh CratonVM process through `spring-boot-suite-runner` against
`C:\craton\CratonVM\apps\spring-boot`.

| Module | Class | JIT | no-JIT |
|---|---|---:|---:|
| `module/spring-boot-transaction` | `JtaAutoConfigurationTests` | 6/6 | 6/6 |
| `module/spring-boot-jdbc-test` | `TestDatabaseAutoConfigurationNoEmbeddedTests` | 2/2 | 2/2 |
| `module/spring-boot-devtools` | `DevToolsEmbeddedDataSourceAutoConfigurationTests` | 4/4 | 4/4 |
| `module/spring-boot-jetty` | `JettyServletWebServerServletContextListenerTests` | 2/2 | 2/2 |

This is 14/14 tests in each execution mode, with zero failures, aborts,
skips, or container failures. A matching HotSpot JIT baseline passed the JTA
class 6/6. The focused native-builtins registration regression test also
passed.

## Scope

This document supersedes the 2026-07-23 hypothesis that both listed classes
still reproduced a Mockito self-attach defect. The four related cold-process
fixtures now pass on current `dev`; there is no remaining issue to track here.
