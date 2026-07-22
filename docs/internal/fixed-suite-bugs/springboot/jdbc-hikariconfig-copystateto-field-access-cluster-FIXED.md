# HikariConfig copyStateTo private-final field access and suspend residual - fixed

**Status: Fixed 2026-07-18**

## Root causes

`HikariConfig.copyStateTo` reads its own declared fields reflectively.  It
does not call `setAccessible(true)` for final fields because it only reads
them.  CratonVM treated every non-public `Field.get*` and `Field.set*` as
inaccessible unless the override flag was set, so that ordinary declaring-class
access failed for Hikari's private-final `AtomicReference` field.

After that access check was corrected, the same lifecycle path exposed a
second gap: `SuspendResumeLock.suspend()` calls
`Semaphore.acquireUninterruptibly(int)`, but the `(I)V` overload was absent
from both normal and surefire Semaphore native registrations.  Execution then
fell through to the unsupported `AbstractQueuedSynchronizer` path.

## Fix

- Field reflection now permits a declaring class to access its own non-public
  field, while retaining the existing override-or-public policy for other
  callers and preserving the module check.
- Registered `Semaphore.acquireUninterruptibly(int)` in both native registries.
- Added focused reflection and semaphore regression coverage.

## Verification

The exact Spring Boot 7.0.2 fixture class
`org.springframework.boot.jdbc.HikariCheckpointRestoreLifecycleTests` passes
all 6 of 6 methods on the source-matched release binary with JIT enabled and
with `--nojit`.  Three post-fix no-JIT confirmation runs also passed 6 of 6.
The equivalent HotSpot run passes 6 of 6 as well.

The release build used a unique target directory and binary name.  Current
`dev` also contains an unrelated non-exhaustive `StartConnect` match in
`native-io/src/socket_channel.rs`; a temporary local compile arm was used only
to construct the test binary and was removed before this change was committed.
It is not part of this fix.
