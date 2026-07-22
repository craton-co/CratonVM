# Spring Boot JDBC/mail custom `InitialContextFactory` cluster — fixed 2026-07-18

**Status: FIXED**

The original report found that CratonVM's native `InitialContext.lookup`
intercept ignored a plain `java.naming.factory.initial` provider. Consequently,
Spring Boot's `TestableInitialContextFactory` bound names into its Java-side
context while CratonVM instead queried its unrelated WildFly-style flat store.

## Correction

`native-builtins/src/wildfly_naming.rs` now materializes the configured factory
property in an environment `Hashtable` and obtains the provider context through
`NamingManager.getInitialContext`. Both `lookup(String)` and `lookup(Name)`
delegate to that returned context after the explicit builder and `java:` URL
context paths have been considered. The native `InitialContext.getEnvironment`
path also exposes the configured factory, matching the real constructor's
system-property behavior.

This correction was integrated in `2f1138488` (`Fix Spring Boot
auto-configuration residuals`), but the JDBC/mail issue record was left open.
This closure independently rebuilt the current integration head and reran the
original cases plus the two directly related JNDI residual classes.

## Verification

Fixture: `apps/spring-boot`, launched through
`apps/spring-boot-suite-runner` with Eclipse Adoptium JDK 25.0.3.9 and the
isolated binary
`cratonvm-jndi-initialcontextfactory-closure-20260718-019f742b.exe`.

| Class | Tests | JIT | `--nojit` |
|---|---:|:---:|:---:|
| `JndiDataSourceAutoConfigurationTests` | 4 | PASS | PASS |
| `MailSenderAutoConfigurationTests` | 19 | PASS | PASS |
| `ConditionalOnJndiTests` | 6 | PASS | PASS |
| `JndiConnectionFactoryAutoConfigurationTests` | 6 | PASS | PASS |

All 35 tests passed in each mode with zero failed, aborted, or failed
containers. Result files are retained in
`apps/spring-boot-suite-runner/.suite-jndi-initialcontextfactory-closure-20260718-019f742b/`.

## Build-integrity residual closed during verification

The current `dev` source also had an incomplete match in the bound
`SocketChannel` connect path after `StartConnect::DeferredFailure` was added.
`native-io/src/socket_channel.rs` now preserves the terminal socket and
surfaces the failure through the existing non-blocking channel contract, while
blocking connects report it immediately. The isolated release build completed
successfully with this correction.
