# Hibernate `SerializationHelperTest` scheduled-executor close residual

**Status:** FIXED 2026-07-21.

## Symptom

After both methods of `org.hibernate.orm.test.util.SerializationHelperTest` passed,
JUnit 6 failed while closing its extension context:

```text
org.junit.platform.commons.JUnitException: Scheduled executor could not be stopped in an orderly manner
```

The preceding explicit-null `Class.forName` loader-scope defect was independently
fixed in `570c51ab4`; this was a separate executor-lifecycle residual.

## Root cause and resolution

JUnit's same-thread timeout watcher schedules a long-delay watchdog task, cancels
it after the invocation, then calls `shutdown()` and `awaitTermination()`. CratonVM's
real `ThreadPoolExecutor` shutdown bridge transitioned `ctl` and interrupted workers,
but skipped the real `ScheduledThreadPoolExecutor.onShutdown()` cleanup. The cancelled
delayed watchdog therefore remained in the queue, so the executor could not terminate
until its original timeout elapsed.

`transition_real_executor_to_shutdown()` now invokes the real dynamic `onShutdown()`
body and the real `ThreadPoolExecutor.tryTerminate()` after the existing state transition
and worker interruption. The cleanup removes cancelled delayed tasks and signals
termination without synthetic field-slot writes.

## Regression coverage and validation

`ScheduledExecutorCancelShutdownProbe` reproduces the exact lifecycle with a
cancelled 120-second delayed task. Its Rust integration test passed on Azure using the
new binary and JDK 25.

The isolated Hibernate `SerializationHelperTest` also passed completely in both modes:

```text
JIT:     found=2 started=2 ok=2 failed=0; @@DONE
--nojit: found=2 started=2 ok=2 failed=0; @@DONE
```

Neither run emitted the JUnit executor-close exception.
