# FIXED — `java/io/Console.ttyStatus()I` native registration — `Console.<clinit>` under Mockito instrumentation

**Status: FIXED 2026-07-28.**

## Symptom

## Superseding closure — FIXED 2026-07-28

`native-builtins/src/lib.rs` now registers `java/io/Console.ttyStatus()I`
and returns the non-interactive status. The sole affected class,
`DefaultLogbackConfigurationTests`, now passes its complete 7-test class in
fresh CratonVM JIT and `--nojit` processes, with a matching HotSpot control.
This was revalidated as part of the fixed
`core-spring-boot-uncategorized-residuals-20260723` aggregate.

```
org.mockito.exceptions.base.MockitoException:
Mockito cannot mock this class: class java.io.Console.
...
Underlying exception : org.mockito.exceptions.base.MockitoException: Cannot instrument class java.io.Console because it or one of its supertypes could not be initialized
	at org.springframework.boot.logging.logback.DefaultLogbackConfigurationTests.consoleLogCharsetShouldUseConsoleCharsetIfConsoleAvailable(DefaultLogbackConfigurationTests.java:57)
     Caused by: org.mockito.exceptions.base.MockitoException: Cannot instrument class java.io.Console because it or one of its supertypes could not be initialized
       ...
     Caused by: java.lang.UnsatisfiedLinkError: java/io/Console.ttyStatus()I
       java.io.Console.<clinit>(Console.java:562)
       org.mockito.internal.creation.bytebuddy.InlineBytecodeGenerator.assureInitialization(InlineBytecodeGenerator.java:257)
```

`DefaultLogbackConfigurationTests.consoleLogCharsetShouldUseConsoleCharsetIfConsoleAvailable()`
calls `Mockito.mock(Console.class)`, which retransforms `java.io.Console`
process-wide (inline mock maker) and, in doing so, drives its `<clinit>` —
which throws `UnsatisfiedLinkError` for a missing native.

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard3/logs/core_spring-boot.org.springframework.boot.logging.logback.DefaultLogbackConfigurationTests.out.log`

## Root cause

Confirmed at file:line. CratonVM registers `java/io/Console.istty()Z`
(`native-builtins/src/lib.rs:27784-27786`) with an explicit comment
(`lib.rs:27773-27807`) claiming this and `JdkConsoleImpl.echo(Z)Z` are the
**only two** natives needed across the whole `System.console()` chain in
JDK 25. That's true for the normal `System.console()` call path (`istty`
gates whether `JdkConsoleImpl` is even constructed), but it misses that
`java.io.Console.<clinit>` itself calls `ttyStatus()I` — a separate native,
never registered anywhere in `native-builtins/src/`. Under normal
operation this is silently masked (`<clinit>` failures on natives are
commonly swallowed/deferred elsewhere), but Mockito's inline mock maker
explicitly forces class initialization before retransforming
(`InlineBytecodeGenerator.assureInitialization`), which surfaces the
`UnsatisfiedLinkError` as a hard `MockitoException` instead.

**Historical fix direction (now applied):** register
`java/io/Console.ttyStatus()I` alongside the existing `istty()Z` — same
non-interactive-embedding rationale, likely just returning a fixed
"not a tty" status code.

`DefaultLogbackConfigurationTests` has two other, unrelated failures in the
same run (`fileLogCharsetShouldUseSystemPropertyIfSet`,
`consoleLogCharsetShouldDefaultToUtf8WhenConsoleIsNull`) — see
`core-spring-boot-uncategorized-residuals-20260723.md`.

## Affected classes

| Module | Class |
|---|---|
| core/spring-boot | org.springframework.boot.logging.logback.DefaultLogbackConfigurationTests (1 of 3 failures: `consoleLogCharsetShouldUseConsoleCharsetIfConsoleAvailable`) |
