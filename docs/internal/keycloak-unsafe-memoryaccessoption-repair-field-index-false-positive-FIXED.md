# `sun.misc.Unsafe.MEMORY_ACCESS_OPTION` repair: nested enum loading fixed

Status: fixed 2026-07-14.

The reported field-index explanation was disproven: the static-only,
declaration-order index used by the repair matches normal static resolution.
The real failure was that `sun/misc/Unsafe$MemoryAccessOption` was not loaded
when `sun/misc/Unsafe`'s post-clinit repair ran. Its lookup-only class-manager
query therefore had no enum constant to copy, leaving
`MEMORY_ACCESS_OPTION` null and causing `Unsafe.putOrderedLong` to dereference
null through `beforeMemoryAccessSlow()`.

## Fix

After `sun/misc/Unsafe` reaches its terminal `Initialized` state, the VM now
loads and initializes `Unsafe$MemoryAccessOption` before the idempotent repair
copies the configured enum constant into `MEMORY_ACCESS_OPTION`. This ordering
is essential: attempting the same work from inside `post_clinit_fixup` before
`Unsafe` is finalized can deadlock on the active class-initialization claim.

The repair continues to preserve the configured policy and only overwrites a
missing or invalid value. It also retains the earlier hardening that verifies a
non-null static slot really holds a `MemoryAccessOption` instance.

## Verification on Azure host

Using `/data/build-keycloak-memaccess-complete-20260714/`
`cratonvm-keycloak-memaccess-complete-20260714-r2` with real JDK 25:

- Reflection probe, default policy: `MEMORY_ACCESS_CONFIGURED_OPTION=ALLOW`,
  `repaired=true`, and `Unsafe.putOrderedLong` completed successfully.
- Reflection probe, `-Dsun.misc.unsafe.memory.access=warn`:
  `MEMORY_ACCESS_CONFIGURED_OPTION=WARN`, `repaired=true`, and the ordered
  write completed with JDK-compatible warning behavior.
- The real Keycloak `ConcurrentAuthzTest` model bootstrap advanced through the
  former Infinispan/Netty Unsafe failure. No
  `MEMORY_ACCESS_OPTION` null dereference remains.

The Keycloak run now reaches later, independent JMX paths. The first such
residual (`VMManagementImpl.getVersion0`) was corrected in the same change by
registering its real-JDK natives as `NativeKind::Bridge`; the next independent
failure is tracked separately in
`docs/known-issues/keycloak/management-notificationemittersupport-listenerlock-null.md`.
