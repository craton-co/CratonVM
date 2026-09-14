# Quarkus: `IllegalStateException: SRCFG00017: Configuration already registered for the given class loader`

## Status
**OPEN, root-caused.** Discovered during Quarkus `NOSTART` classes 4-shard re-run (`runs/run-20260915-151125-nostart-4shards`).

## Symptom
Batch test launcher processes abort mid-run with an unhandled exception in `ConfigLauncherSession`:

```
java.lang.IllegalStateException: SRCFG00017: Configuration already registered for the given class loader
	at io.smallrye.config.SmallRyeConfigProviderResolver.registerConfig(SmallRyeConfigProviderResolver.java:145)
	at io.quarkus.test.config.ConfigLauncherSession.launcherSessionOpened(ConfigLauncherSession.java:42)
	at org.junit.platform.launcher.listeners.session.CompositeLauncherSessionListener.lambda$launcherSessionOpened$0(CompositeLauncherSessionListener.java:34)
	at java.util.ArrayList.forEach(ArrayList.java:1604)
	at org.junit.platform.launcher.listeners.session.CompositeLauncherSessionListener.launcherSessionOpened(CompositeLauncherSessionListener.java:34)
	at org.junit.platform.launcher.core.DefaultLauncherSession.<init>(DefaultLauncherSession.java:71)
	at org.junit.platform.launcher.core.SessionPerRequestLauncher.createSession(SessionPerRequestLauncher.java:99)
	at org.junit.platform.launcher.core.SessionPerRequestLauncher.execute(SessionPerRequestLauncher.java:75)
	at CratonRunner.main(CratonRunner.java:54)
```

## Root Cause
When `CratonRunner` executes a batch of Quarkus tests sequentially within a single VM process:
1. `SmallRyeConfigProviderResolver` registers a global `SmallRyeConfig` instance bound to the thread context ClassLoader (`AppClassLoader`).
2. When a test completes or a new test session is opened without releasing or unregistering the SmallRye config provider from `SmallRyeConfigProviderResolver`, opening a new `LauncherSession` attempts to re-register the configuration for the same `ClassLoader`.
3. `SmallRyeConfigProviderResolver.registerConfig` detects the duplicate registration for the ClassLoader key and throws `IllegalStateException: SRCFG00017`.
4. In CratonVM, because ClassLoader identity or static provider maps persist across test runner iterations without isolated child loaders or release hooks, batch execution terminates prematurely.

## Affected Tests / Scenarios
- Multi-class batch execution runs in `apps/quarkus-suite-runner` when sequential test classes initialize Quarkus `ConfigLauncherSession`.

## Remediation / Solution Plan
1. In `CratonRunner` harness (or `SmallRyeConfigProviderResolver` native bridge), call `ConfigProviderResolver.instance().releaseConfig(config)` or reset the provider map between class executions.
2. Verify ClassLoader isolation semantics in `native-builtins/src/classloader.rs` for `Thread.currentThread().getContextClassLoader()`.
