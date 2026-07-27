# `ScheduledExecutorService` worker threads get the literal name `"Thread"` instead of `pool-N-thread-M`

**Status: OPEN — found 2026-07-23**

## Symptom

`core/spring-boot-autoconfigure`'s `TaskSchedulingAutoConfigurationTests`
passes 15/16; `enableSchedulingWithExistingScheduledExecutorServiceBacksOff()`
fails:

```
=> java.lang.AssertionError:
Expecting all elements of:
  ["Thread"]
to match given predicate but this element did not:
  "Thread"
       org.springframework.boot.autoconfigure.task.TaskSchedulingAutoConfigurationTests.lambda$enableSchedulingWithExistingScheduledExecutorServiceBacksOff$0(TaskSchedulingAutoConfigurationTests.java:257)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard3/logs/core_spring-boot-autoconfigure.org.springframework.boot.autoconfigure.task.TaskSchedulingAutoC-8cf107ed74e2.out.log`

The test (`TaskSchedulingAutoConfigurationTests.java:249-259`) registers a
user-supplied `ScheduledExecutorService` bean built with
`Executors.newScheduledThreadPool(2)` (`ScheduledExecutorServiceConfiguration`,
`.java:294-302`), runs a scheduled task on it, and asserts every thread name
observed while the task ran contains `"pool-"` (the standard JDK
`Executors.DefaultThreadFactory` naming scheme, `"pool-" + poolNumber +
"-thread-" + threadNumber`). On CratonVM, the observed thread name is the
literal string `"Thread"` — not `"pool-1-thread-1"`, not even the JDK's
anonymous-`Thread` fallback shape (`"Thread-0"`, `"Thread-1"`, ...) — just
the bare word with no numeric suffix at all.

This is unrelated to the class's other previously-documented failure mode
(the "Invalid destruction signature" `Class.getMethods()` override-shadowing
bug, [`../../internal/fixed-suite-bugs/springboot/taskscheduling-invalid-destruction-signature-recurrence-FIXED.md`](../../internal/fixed-suite-bugs/springboot/taskscheduling-invalid-destruction-signature-recurrence-FIXED.md))
— that doc's affected test method and error text are both different, and
that bug is confirmed fixed (15/16 tests pass here, including the specific
bean-destruction path that doc covers).

## Root cause — confirmed at the source level

`native-builtins/src/lib.rs` registers several native constructors for
`java/lang/Thread` that hardcode the literal string `"Thread"` as the
thread's name whenever no name is supplied to that specific overload,
instead of computing the real JDK's auto-generated
`"Thread-" + Thread.nextThreadNum()` (an internal static counter incremented
per anonymously-constructed `Thread`):

```rust
// native-builtins/src/lib.rs:32467-32480
registry.register("java/lang/Thread", "<init>", "()V", |ctx, args| {
    ...
    let name = Value::Object(Some(ctx.create_string("Thread")));
    ...
});
```

and identically for the `(Runnable)V` overload
(`native-builtins/src/lib.rs:32481-32501`). `Executors.DefaultThreadFactory`
(real JDK `Executors` inner class) always explicitly computes and passes a
name string via the `Thread(ThreadGroup, Runnable, String, long)`
constructor — so if that exact bytecode path ran unmodified, the resulting
thread names would be correct regardless of what the no-arg/`(Runnable)`
constructors do. The observed literal `"Thread"` name for a
`newScheduledThreadPool` worker is therefore strong evidence that
`ScheduledThreadPoolExecutor`'s (or `ThreadPoolExecutor`'s) worker-thread
creation on CratonVM does **not** go through the real
`DefaultThreadFactory.newThread(Runnable)` bytecode for this executor
configuration, and instead reaches a `Thread` construction path that lands
on one of these two hardcoded-name native registrations. `native-builtins/src/lib.rs`
contains a substantial amount of both fully-native (`alloc_concurrent_synthetic`)
and bytecode-executing (`invoke_virtual_bytecode_only`-routed) handling for
`ThreadPoolExecutor`/`ScheduledThreadPoolExecutor` (see e.g. lines ~69229,
~69616-69634, ~69837-69947) — which specific path this particular
`Executors.newScheduledThreadPool(2)` configuration takes, and why it
bypasses the real `DefaultThreadFactory`, is **not confirmed** in this
session (would require tracing which `ThreadPoolExecutor`/`Worker`
construction route `newScheduledThreadPool` resolves to on CratonVM and
whether it calls `getThreadFactory().newThread(...)` at all).

**What would confirm/refute this:** add temporary tracing in the two
`Thread.<init>` native registrations above (log the calling context / a
short backtrace when `name == "Thread"` is about to be set) and re-run this
test; if the caller is native pool-worker-allocation code rather than
interpreted `DefaultThreadFactory` bytecode, that confirms the executor's
worker-creation path is the bypass point, not the `Thread` constructors
themselves (which only need a real default name as a fallback — the bug is
that the fallback is reached at all in this case).

## Affected classes

| Module | Class | Method |
|---|---|---|
| `core/spring-boot-autoconfigure` | `org.springframework.boot.autoconfigure.task.TaskSchedulingAutoConfigurationTests` | `enableSchedulingWithExistingScheduledExecutorServiceBacksOff` (1 of 16 tests) |
