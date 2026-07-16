# `sun.management.NotificationEmitterSupport.listenerLock` null NPE building Micrometer JVM metrics (Keycloak/Infinispan)

Status: open

Date observed: 2026-07-14, while re-verifying the (now-fixed) `Unsafe.MEMORY_ACCESS_OPTION` repair.

## Repro

Run Keycloak 26.6.1 `testsuite/model`'s `org.keycloak.testsuite.model.authz.ConcurrentAuthzTest` under the
real-JDK CratonVM harness with `Infinispan,Jpa` model parameters. Model bootstrap now passes the former
Unsafe and `VMManagementImpl.getVersion0` native-linkage blockers (both fixed — see
`unsafe-memoryaccessoption-repair-field-index-false-positive-FIXED.md` in git history / commits `776b8cb0`,
`f35c0872`, `9fc7d2ef`), then fails while Infinispan constructs Micrometer JVM metrics.

## Failure

`io.micrometer.core.instrument.binder.jvm.JvmHeapPressureMetrics` registers a listener on a JMX memory-pool
emitter. Real bytecode enters `sun.management.NotificationEmitterSupport.addNotificationListener`, which throws:

```text
java.lang.NullPointerException: Cannot enter synchronized block because
"this.listenerLock" is null
    at sun.management.NotificationEmitterSupport.addNotificationListener
    at io.micrometer.core.instrument.binder.jvm.JvmHeapPressureMetrics.monitor
```

## Root cause (hypothesis)

The JMX object exposed to Micrometer was allocated without the real `NotificationEmitterSupport` superclass
constructor running, so `listenerLock` (and presumably the listener collection) never get their JDK-compatible
initial values. This is independent of the fixed Unsafe repair: the run now gets farther precisely because
`MEMORY_ACCESS_OPTION` is repaired and `VMManagementImpl.getVersion0` is dispatchable — this is the next
blocker exposed once those two were cleared.

## Next steps

1. Trace the allocation path for the `MemoryPoolMXBean`/notification-emitter object CratonVM hands to Micrometer
   and determine whether it goes through a synthetic allocation path that skips `NotificationEmitterSupport`'s
   real constructor (likely `native-builtins/src/jmx.rs`, wherever `MemoryPoolMXBean`/`NotificationEmitterSupport`
   objects are constructed for the platform MBean server).
2. Either route construction through the real superclass constructor (`invoke_special` into
   `NotificationEmitterSupport.<init>`), or explicitly initialize the inherited `listenerLock` and listener
   collection fields with their JDK-compatible values at synthetic-allocation time.
3. Add a focused real-JDK regression test that calls `addNotificationListener` on the same memory-pool object
   before rerunning the Keycloak model class, to confirm the fix and guard against regression.
