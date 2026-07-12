# FIXED: Spring Boot `Unsafe$MemoryAccessOption.ordinal()` NPE cluster

Status: fixed and retired on 2026-07-12.

## Cause

On JDK 25, `sun.misc.Unsafe.<clinit>` caches the result of
`Unsafe$MemoryAccessOption.value()` in `MEMORY_ACCESS_OPTION`. During
real-JDK bootstrap, CratonVM could complete that initialization with the
outer static still null. The first legacy Unsafe memory access then entered
`beforeMemoryAccessSlow()` and dereferenced the null value via `ordinal()`.

This directly broke Couchbase and Lettuce construction, and indirectly broke
Netty's `NioEventLoopGroup` while it initialized its MPSC task queue.

## Resolution

`9fc7d2ef` added a successful-class-initialization repair in
`vm/src/vm/vm_util.rs`. If `sun/misc/Unsafe.MEMORY_ACCESS_OPTION` is missing,
the repair selects the initialized nested-enum value for the configured policy
and leaves an existing value untouched.

## Validation

Fresh Azure-host build from `origin/dev`:

- Worktree: `/data/wt-spring-unsafe-memoryaccessoption-20260712-192831`
- Target directory: `/data/data/target-spring-unsafe-memoryaccessoption-20260712-192831`
- Binary: `/data/data/cratonvm-bins/cratonvm-spring-unsafe-memoryaccessoption-20260712-192831`

Focused regressions on that binary:

1. A reflection-based `Unsafe.putOrderedLong` probe completed with
   `UNSAFE_PUT_ORDERED_LONG_OK value=42`, exercising the direct failing
   legacy-Unsafe path.
2. A real Netty 4.1.108 `NioEventLoopGroup(1)` probe completed with
   `NETTY_NIO_EVENT_LOOP_GROUP_OK`, exercising the downstream event-loop and
   MPSC queue construction path reported by the Spring Boot failures.

Both the direct `ordinal()` NPE and the indirect Netty child-event-loop
failure are therefore retired.
