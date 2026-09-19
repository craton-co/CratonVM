# Continuation-backed virtual threads

CratonVM virtual threads now execute on a bounded carrier pool. Each virtual
thread owns a boxed `JvmThread`; its Java frames and precise root state remain
heap-resident while the continuation is unmounted. No native stack or dedicated
OS thread is retained across an unpinned sleep, park, or supported keyed wait.

## Scheduler protocol

1. `Thread.start` registers the Java thread and installs a boxed virtual runtime.
2. A carrier mounts that runtime and invokes `Thread.run`.
3. An unpinned blocking native returns the internal `ContinuationYield` signal.
4. Every interpreter boundary preserves its current frame, PC, locals, and
   operand stack while the signal unwinds the Rust carrier stack.
5. The manager publishes the stable runtime address for precise GC scanning,
   retires its TLAB, and either arms the shared timer or records a keyed wait.
6. A timer, `unpark`, or condition transition resubmits the continuation.
7. A carrier remounts the same boxed runtime and resumes from the youngest
   preserved frame through its callers.

The yield signal is not a Java exception and is intercepted before exception
translation or ordinary error unwinding. Pinned virtual threads retain the
blocking fallback and emit the existing pinned-thread event.

Virtual-thread activations remain interpreted even when the process JIT is
enabled. The compiler has precise deoptimization metadata, but does not yet
materialize an arbitrary compiled activation directly into a continuation
stack chunk. All JIT entry paths therefore use a per-thread continuation gate;
platform threads still compile and execute the same shared methods. This avoids
introducing method-name exclusions and preserves exact frames until compiled
stack-chunk deoptimization is implemented.

## Keyed waits and race ordering

The synthetic `CountDownLatch` implementation stores a VM-local stable wait key
beside its count. A virtual `await()` registers its continuation, rechecks the
count to close the registration race, and unmounts only while the count remains
positive. The zero transition drains the keyed waiter set.

If a zero transition wins before the carrier has deposited the continuation,
the manager records `wake_pending`. `suspend_runtime` publishes the stack first,
then consumes that flag and resubmits it. If suspension wins first, the wake
submits the already-published continuation directly. Thus no wake can be lost
and a moving GC never depends on an object-address side-table key.

## Validation

The checked-in `vm/tests/resources/vthread_probe` fixtures cover builder/start/join and
10,000 sleeping virtual threads. The external deep-frame probe additionally
holds live category-2 values through a timed unmount and checks an exact
checksum after resumption. Delivery validation runs these probes with JIT
enabled and with `--nojit`, while sampling `/proc/<pid>/status` to confirm that
native thread count remains bounded by the carrier pool rather than virtual
thread count.
