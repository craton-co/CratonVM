# Keycloak/Infinispan metrics: synthetic JMX notification emitter leaves `listenerLock` null

Status: open. Observed 2026-07-14 while re-verifying the fixed
`Unsafe.MEMORY_ACCESS_OPTION` repair.

## Repro

Run Keycloak 26.6.1 `testsuite/model`
`org.keycloak.testsuite.model.authz.ConcurrentAuthzTest` under the real-JDK
CratonVM harness with `Infinispan,Jpa` model parameters. The model bootstrap
now passes the former Unsafe and VMManagement native-linkage blockers, then
fails while Infinispan constructs Micrometer JVM metrics.

## Failure

`io.micrometer.core.instrument.binder.jvm.JvmHeapPressureMetrics` registers a
listener on a JMX memory-pool emitter. Real bytecode enters
`sun.management.NotificationEmitterSupport.addNotificationListener`, which
throws:

```text
java.lang.NullPointerException: Cannot enter synchronized block because
"this.listenerLock" is null
    at sun.management.NotificationEmitterSupport.addNotificationListener
    at io.micrometer.core.instrument.binder.jvm.JvmHeapPressureMetrics.monitor
```

The JMX object exposed to Micrometer was allocated without the real superclass
constructor establishing `NotificationEmitterSupport.listenerLock`. This is
independent of the fixed Unsafe repair: the run now gets farther precisely
because `MEMORY_ACCESS_OPTION` is repaired and `VMManagementImpl.getVersion0`
is dispatchable.

## Next step

Trace the allocation path for the `MemoryPoolMXBean`/notification-emitter
object and either run the real constructor or initialize the inherited
`listenerLock` and listener collection with their JDK-compatible values. Add a
focused real-JDK regression that calls `addNotificationListener` on the same
memory-pool object before rerunning the Keycloak model class.
