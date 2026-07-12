# FIXED: `testsuite/model` Unsafe ordered-store NPE blocked Netty event loops

Status: fixed and retired on 2026-07-12.

## Cause

On JDK 25, `sun.misc.Unsafe.<clinit>` stores the result of
`MemoryAccessOption.value()` in `MEMORY_ACCESS_OPTION`. CratonVM correctly
exposed `sun.misc.unsafe.memory.access=allow`, and the nested enum correctly
resolved `ALLOW`, but the outer static field was still left null. The next
legacy Unsafe access therefore reached `beforeMemoryAccessSlow()` and failed
at `MEMORY_ACCESS_OPTION.ordinal()`.

Netty's MPSC queue initialization calls `Unsafe.putOrderedLong`, so embedded
Infinispan/Keycloak initialization failed while creating its event-loop group.

## Resolution

`vm/src/vm/vm_util.rs` now repairs a missing
`sun/misc/Unsafe.MEMORY_ACCESS_OPTION` after successful class initialization.
The repair selects the already-initialized enum constant matching the effective
saved-property policy (`allow`, `warn`, `debug`, or `deny`) and does not
overwrite a non-null value.

## Validation

Fresh Azure-host build:

- Worktree: `/data/data/wt-keycloak-unsafe-putorderedlong-20260712`
- Target directory: `/data/data/target-keycloak-unsafe-putorderedlong-20260712`
- Binary: `/data/data/cratonvm-bins/cratonvm-keycloak-unsafe-putorderedlong-20260712`

Focused probes on that binary:

1. Reflection-based `Unsafe.putOrderedLong` probe reported
   `MEMORY_ACCESS_CONFIGURED_OPTION=ALLOW` and
   `UNSAFE_PUT_ORDERED_LONG_OK memoryAccess=allow`.
2. A real Netty `NioEventLoopGroup(1)` probe reported
   `NETTY_NIO_EVENT_LOOP_GROUP_OK`, covering the MPSC task-queue construction
   that had blocked Keycloak's Infinispan startup.
3. `-Dsun.misc.unsafe.memory.access=deny` resolved the repaired static to
   `DENY` and correctly produced `UnsupportedOperationException` from
   `putOrderedLong`, confirming that explicit policy overrides remain intact.

The Azure host does not currently contain compiled `testsuite/model`
`UserModelTest` artifacts (nor a Linux-valid Keycloak universal classpath), so
the final suite-level check was unavailable. The direct Unsafe and real Netty
probes exercise the exact failing call and downstream queue-construction path.
