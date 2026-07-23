# `DevToolPropertiesIntegrationTests` / `DevToolsEmbeddedDataSourceAutoConfigurationTests`

**Status: OPEN — investigated 2026-07-23.** Both classes are part of the
`module/spring-boot-devtools` 4-class residual set from the 2026-07-23
429-class rerun (`apps/spring-boot-suite-runner/RESULTS-20260723.md`), which
recorded both failing fast (~300-500ms) with `TestEngine with ID
'junit-jupiter' encountered a critical issue during test discovery` /
`UniqueIdSelector ... could not be resolved` for every test method. That
exact signature was **not** reproduced this session, despite many retries —
see below for what was found instead. This shared build box was heavily
oversubscribed throughout this investigation (10-16+ concurrent
`rustc`/`cargo` processes from other sessions, 400+ total processes,
`SbRunner` startup alone regularly taking 90-160s instead of ~2s), which
makes clean signal hard to get; both symptoms below are more likely real
than initially assumed (see reasoning per class), but neither has been
independently root-caused to a specific file:line.

## `DevToolPropertiesIntegrationTests` — genuine hang, confirmed

Reproduced a hard hang **3 times**, including once with a 1200-second
(20-minute) timeout, fully isolated (`-Parallel 1`), on a host that was
otherwise healthy enough to pass 47/51 other classes in the same module in
under 10 minutes. `SbRunner` itself starts successfully (~90-something
seconds, itself slower than ideal but not the issue) and prints "Started
SbRunner", then **zero further output ever appears** — the process is
genuinely stuck before or during the very first `@Test` method, not merely
slow. This is not host-load noise.

**Leading hypothesis (not confirmed):** every test method in this class
uses the shared `getContext(Supplier<ConfigurableApplicationContext>)`
helper:

```java
protected ConfigurableApplicationContext getContext(Supplier<ConfigurableApplicationContext> supplier)
        throws Exception {
    AtomicReference<ConfigurableApplicationContext> atomicReference = new AtomicReference<>();
    Thread thread = new Thread(() -> {
        ConfigurableApplicationContext context = supplier.get();
        atomicReference.getAndSet(context);
    });
    thread.start();
    thread.join();
    ...
}
```

This spawns a **new** `Thread` to run `application.run()` and then blocks
the calling (forked) thread on `thread.join()`. Per JDK spec, a newly
created `Thread` inherits its *creating* thread's context classloader at
construction time — here, that should be `ModifiedClassPathClassLoader`
(since `@ForkedClassPath` already switched the calling thread's context
classloader before `runTest()`/the test body runs). If CratonVM's `new
Thread(Runnable)` does not correctly propagate the parent thread's
*current* context classloader, or if CratonVM's classloader-definition
locking (`ucl_try_define_local_class`'s per-`(loader, name)` mutex in
`native-builtins/src/classloader.rs`, or a similar lock elsewhere) is
acquired in different orders by two concurrently-active threads resolving
classes through the same isolated loader, a genuine deadlock on
`thread.join()` is plausible. **Not verified** — would need a thread-dump
capture at the moment of the hang (this box has no attachable debugger per
prior session notes) or, more practically, targeted tracing of every lock
acquisition during `getContext()`'s spawned thread's Spring context
creation, compared against `RestartApplicationListenerTests` (a *passing*
class in the same module, worth checking whether it uses the same
spawn-a-thread pattern — if it does and passes, that would rule out a
blanket "new Thread doesn't inherit context classloader" theory and point
more specifically at lock ordering).

## `DevToolsEmbeddedDataSourceAutoConfigurationTests` — reproducible Mockito failure

Failed consistently (3/3 runs, including in the healthy 51-class
full-module run where 47/51 other classes passed cleanly) with:

```
java.lang.IllegalStateException: Could not initialize plugin: interface org.mockito.plugins.MockMaker (alternate: null)
Caused by: java.lang.IllegalStateException: Internal problem occurred, please report it. Mockito is unable to load the default implementation of class that is a part of Mockito distribution. Failed to load interface org.mockito.plugins.MockMaker
Caused by: java.lang.reflect.InvocationTargetException: java.lang.NoClassDefFoundError: org.mockito.internal.creation.bytebuddy.MockMethodAdvice
Caused by: java.lang.NoClassDefFoundError
```

All 4 test methods fail identically (`SBRUNNER_RESULT tests=4 failed=4`).
This class has `@ClassPathExclusions("HikariCP-*.jar")` at the class level —
same fork mechanism family as the other 3 residual classes in this module,
though the failure mode here is distinctly Mockito's own inline-mock-maker
self-attach (`ByteBuddyAgent`/JVM Attach API), not a JUnit discovery issue
or an `IllegalArgumentException`. `NoClassDefFoundError` for a class that
exists on disk is the JVM's "erroneous class" signal (JVMS §5.5): some
*earlier* attempt to initialize `MockMethodAdvice` (or a class in its
`<clinit>` chain) threw, and every subsequent reference is now permanently
poisoned for that classloader — the actual root exception from that first
failure was not captured this session. Given the reproducibility (not
flaky across identical re-runs) this looks like a real, deterministic
issue in this exact class/fork combination rather than pure load noise, but
was not root-caused further — the original 2026-07-23 rerun's
"could not be resolved" signature for this class was also not reproduced,
so it's unclear whether these are the same underlying bug manifesting
differently or two distinct issues.

## Suggested next steps

- Attach a debugger or add lock-acquisition tracing to find exactly where
  `DevToolPropertiesIntegrationTests` blocks forever.
- For the Mockito failure, capture the *first* `MockMethodAdvice`
  initialization attempt's real exception (e.g. by adding a
  `-Djava.util.logging` filter or a temporary native trace around Mockito's
  self-attach path) rather than the later, already-poisoned
  `NoClassDefFoundError`.
- Re-run both on a genuinely quiet host once available, to confirm whether
  the original `UniqueIdSelector ... could not be resolved` signature still
  reproduces at all on current `dev`, or whether it's been superseded by
  these two different symptoms.

## Reproduce

```powershell
$env:JAVA_HOME = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
$exe = "<worktree>\target\release\cratonvm.exe"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Exe $exe -JdkHome $env:JAVA_HOME `
  -ClassList <tsv, header 'module\tclass', rows for
    module/spring-boot-devtools org.springframework.boot.devtools.env.DevToolPropertiesIntegrationTests and/or
    module/spring-boot-devtools org.springframework.boot.devtools.autoconfigure.DevToolsEmbeddedDataSourceAutoConfigurationTests> `
  -RunName retest -Parallel 1 -TimeoutSec 600
```
