# Threading & Concurrency

CratonVM supports multi-threaded Java programs, object monitors, the
`java.util.concurrent` primitives, and virtual threads. The threading subsystem
lives in the `cratonvm-vm` crate under `vm/src/threading/`.

## Components

| Module | Responsibility |
|--------|----------------|
| `jvm_thread.rs` | Per-thread state (call stack, output, thread-local data). |
| `thread_registry.rs` | Global tracking of all live threads. |
| `monitor.rs` | Object monitors (`synchronized`, `wait`/`notify`/`notifyAll`). |
| `gc_barrier.rs` | Stop-the-world safepoint coordination. |
| `virtual_scheduler.rs` | The virtual-thread scheduler (Java 21+). |

## Platform threads

Each Java thread maps to an OS thread with its own `JvmThread` state (call stack,
printed output, and so on). The thread registry tracks every live thread so the
collector can find their stacks as GC roots.

> When running against a real JDK, make sure it is a modern release — some
> thread-creation paths behave differently on older JDKs.

## Monitors & locks

`synchronized` methods and blocks use object monitors, which provide mutual
exclusion plus the `wait`/`notify`/`notifyAll` condition mechanism. On top of
these, the `java.util.concurrent` locks, latches, semaphores, barriers, and
concurrent collections are supported (see [Standard Library
Coverage](../java-support/standard-library.md)). Some `java.util.concurrent`
constructs are still being brought to full parity — see [Known
Limitations](../java-support/limitations.md).

## Virtual threads

Virtual threads (Java 21+) are scheduled by a dedicated virtual-thread scheduler,
multiplexing many virtual threads onto carrier threads.

## Safepoints & the GC barrier

Garbage collection requires a consistent global view, so it runs at a
**stop-the-world safepoint**: all mutator threads are brought to a known point
before roots are scanned and live objects traced. The GC barrier coordinates
bringing threads to the safepoint and releasing them afterward. This is how the
[garbage collector](garbage-collector.md) gets a stable snapshot of every
thread's stack.

## Lock ordering

Concurrency correctness depends on a disciplined global lock-acquisition order.
CratonVM defines a `LockLevel` hierarchy and runtime-enforcement wrappers, and
documents this **in source** as the canonical reference. Optional runtime
lock-order checking can be enabled in release builds with
`CRATONVM_LOCK_ORDER_CHECK` (it is always on in debug builds). When working on
concurrent code, respect the in-source lock order — it is the source of truth.

## Foreign threads

A foreign OS thread that wants to call into the VM must be registered with the
GC safepoint machinery, not merely the thread registry, or the collector can
stall or corrupt the heap. Full foreign-thread attach is a tracked,
deliberately-out-of-scope item — see [Embedding Overview](../embedding/overview.md)
and [Known Limitations](../java-support/limitations.md).
