# `ScheduledExecutorService` worker names now use the JDK default factory

**Status: FIXED — 2026-07-28**

## Symptom

`core/spring-boot-autoconfigure`'s
`TaskSchedulingAutoConfigurationTests.enableSchedulingWithExistingScheduledExecutorServiceBacksOff()`
observed a worker name that did not contain `pool-` when its
`ScheduledExecutorService` bean was created with
`Executors.newScheduledThreadPool(2)`.

The original report recorded the literal `"Thread"`. Reproduction against the
then-current `dev` on 2026-07-28 reproduced the same underlying regression as
`"Thread-0"`: 15/16 class tests passed and the one affected method failed in
both JIT and `--nojit` modes.

## Root cause

CratonVM replaced both `Executors.defaultThreadFactory()` and the
`ThreadFactory.newThread(Runnable)` interface method with synthetic natives.
It also injected those methods as native into the real-JDK class metadata.
Consequently, even though real JDK `Executors$DefaultThreadFactory` bytecode
constructs `pool-N-thread-M`, the VM returned a bare synthetic
`java.util.concurrent.ThreadFactory`; its worker construction reached the
fallback `Thread(Runnable)` constructor and yielded `Thread-N`.

## Fix

Removed all three layers of the synthetic default-factory override:

- the essential-native registrations;
- the executor-extension registrations; and
- the real-JDK extra-native metadata entries.

The VM now loads and executes the real
`Executors$DefaultThreadFactory`. A focused probe reports the concrete factory
class and `pool-1-thread-1`, matching HotSpot.

## Validation

Azure host (`20.83.144.174`), JDK 25, fresh release build:

- standalone factory probe: `Executors$DefaultThreadFactory` and
  `pool-1-thread-1`;
- `TaskSchedulingAutoConfigurationTests`, JIT: 16 started, 0 failed;
- `TaskSchedulingAutoConfigurationTests`, `--nojit`: 16 started, 0 failed.

The prior invalid-destruction-signature issue remains independently fixed in
`taskscheduling-invalid-destruction-signature-recurrence-FIXED.md`.
