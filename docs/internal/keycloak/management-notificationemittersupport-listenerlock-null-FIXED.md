# Keycloak/Infinispan metrics: `MemoryImpl` notification-emitter initialization fixed

Status: fixed 2026-07-14.

## Root cause

`ManagementFactory.getMemoryMXBean()` returned an object stamped with the
`MemoryMXBean` interface. That is not the concrete JDK implementation and does
not inherit `sun.management.NotificationEmitterSupport`. It therefore could
not satisfy Micrometer's notification-emitter contract. Synthetic allocation
also bypasses `NotificationEmitterSupport`'s constructor, leaving its
`listenerLock` and `listenerList` fields null when a concrete management bean
is used.

## Fix

The management factory now allocates real-JDK `sun.management.MemoryImpl`,
then initializes the inherited listener lock and a real initialized
`ArrayList` before returning the bean. Allocation keeps the emitter rooted
across these operations so a moving collection cannot invalidate the receiver.

The checked-in `apps/jmx_probe/JmxProbe.java` regression validates the complete
contract: it enumerates the MXBean lists, asserts that `MemoryMXBean` is a
`NotificationEmitter`, and adds then removes a listener. The Rust integration
test compiles and runs that probe automatically.

## Verification on Azure

- `CARGO_TARGET_DIR=/data/target-keycloak-listenerlock-20260714-v2 cargo check -p cratonvm-native-builtins`
- Unique release binary probe: `pools=2`, `mgrs=1`, `gcs=1`, `listener=OK`,
  and `OK`.
- `cargo test -p cratonvm-vm --test wave1_a_jmx_mxbeans -- --nocapture`:
  passed (1/1).
- The real Keycloak `ConcurrentAuthzTest` model bootstrap passed the former
  `NotificationEmitterSupport.addNotificationListener` failure. It then
  reached a separate `KeycloakSession.realms()` provider-null failure, tracked
  independently in
  `docs/known-issues/keycloak/keycloak-session-realms-null-after-jmx.md`.