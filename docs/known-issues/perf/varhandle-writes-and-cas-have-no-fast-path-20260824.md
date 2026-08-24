# `VarHandle` writes and CAS have no fast path — 35-303x, and it is most of `java.util.concurrent`

## Status
**PARTIALLY FIXED (2026-08-24).** `set` is bound and verified — 698 000 native
dispatches eliminated, 246.5 ns -> 64.8 ns (see below). `compareAndSet` and
reference READS are still on the generic funnel, and the downstream
composition workload is CAS-dominated so it has barely moved. The defect was a
gap in an existing optimisation rather than a bug: `VARHANDLE_READ_DIRECT_FNS`
bound READS of PRIMITIVE fields and nothing else.

## Severity
**HIGH, and broad.** `VarHandle` is the primitive under `CompletableFuture`,
`AbstractQueuedSynchronizer`, `ConcurrentHashMap`, `ForkJoinPool`,
`ConcurrentLinkedQueue` and `StampedLock`. Anything built on those pays it.

## The measurement

`HibfixVarHandleProbe`, single-threaded, each operation timed on its own after
a shared warmup:

| operation | HotSpot | CratonVM | ratio |
|---|---:|---:|---:|
| `VarHandle.compareAndSet`, reference field | 9.2 ns | **488.9 ns** | **53x** |
| `VarHandle.set`, reference field | 1.0 ns | **303.4 ns** | **303x** |
| `VarHandle.compareAndSet`, `int` field | 8.6 ns | **300.2 ns** | **35x** |
| `AtomicReference.compareAndSet` | 10.9 ns | **911.2 ns** | **84x** |
| `AtomicInteger.incrementAndGet` | 5.0 ns | 6.2 ns | **1.2x** |
| plain field store (baseline) | 1.1 ns | 10.3 ns | 9x |

**`AtomicInteger` is the control that makes this conclusive.** It has its own
intrinsic and is at parity, so this is not "atomics are slow" or "the box is
slow" — it is `VarHandle` specifically. A `VarHandle.set` of a reference costing
303 ns against a 10 ns plain store is a volatile store paying 30x an ordinary
one.

## Why: the fast path is reads-only, by construction

`jit/src/lib.rs` binds `VARHANDLE_READ_DIRECT_FNS` for

* modes `VARHANDLE_READ_MODES = ["get", "getVolatile", "getOpaque", "getAcquire"]`
* returns `VARHANDLE_READ_RETURNS = [Z B C S I J F D]`

and its own doc says of the reference kinds: "`L` and `[` are absent on
purpose". So the table has 32 slots, all of them reads of primitives. It was
built for netty's `RefCnt.isLiveNonVolatile`, which is `(int) VH.get(instance)`
— a read of an `int` — and it does that job.

Writes and CAS were never in scope. They still go through
`jit_invoke_dispatch`, paying the SATB flush, the reference-argument
forwarding, the site-key revalidation and two thread-local map probes that the
read path's own doc describes as the per-call floor it was created to remove.

## What it costs downstream

`HibfixComposeProbe2` composes 4 800 000 `CompletableFuture` chains with no
scheduler, no locks and no cross-thread handoff — each thread owns its futures
end to end:

| | HotSpot | CratonVM | ratio |
|---|---:|---:|---:|
| 4.8 M compose chains | **412 ms** | **359 197 ms** | **~872x** |

`wrong=0` throughout: this is purely cost, not a correctness defect. A stack
profile of that run puts **34.0% in `CompletableFuture.tryPushStack`**, which is

```java
Completion h = stack;
NEXT.set(c, h);                        // VarHandle set, reference field
return STACK.compareAndSet(this, h, c);   // VarHandle CAS, reference field
```

with `UniCompose.tryFire` at 21.4%, `completeRelay` at 12.4% and
`uniComposeStage` at 8.0% behind it. Each thread owns its futures, so that CAS
is **uncontended and succeeds on the first attempt** — a third of the time in
an uncontended CAS is the primitive, not the algorithm. `ForkJoinTask.casStatus`
and `compareAndSetForkJoinTaskTag` appear lower down for the same reason.

## Step 1 landed: `set` is bound (2026-08-24)

The write modes now have the bind the read modes have had:
`VARHANDLE_WRITE_DIRECT_FNS`, 36 slots over
`["set", "setVolatile", "setRelease", "setOpaque"]` x
`[Z B C S I J F D L]`, recognised at BOTH the single-pass and the OSR door.

**References are in scope here, and that is not an oversight in the read
table's direction.** The read bind excludes `L`/`[` because a reference RETURN
must be published as a handoff root before the caller can store it, and the
direct arm takes no thread borrow to publish one with. A write has no return:
the reference travels INWARD, in a register the compiled caller's own frame
already describes, so there is no window to root across. The store itself goes
through `set_field_volatile_as`, which routes to the collector's own inherent
write barrier rather than an open-coded one — so this arm and the interpreter's
`putfield` tell the GC the same story.

Measured on ONE binary with `CRATONVM_JIT_VARHANDLE_WRITE_DIRECT_HELPERS`:

| | bind off | bind on |
|---|---:|---:|
| `VarHandle.set` NATIVE dispatches (census) | **698 000** | **0** |
| `VarHandle.set` reference | 246.5 ns | **64.8 ns** |
| `VarHandle.CAS int` (control, unbound) | 233.8 ns | 239.5 ns |
| `VarHandle.get` natives (control) | 500 000 | 500 000 |
| `compareAndSet` natives (control) | 1 396 000 | 1 396 000 |

The controls are what make it a bind and not a coincidence: every unbound
operation is unchanged, and the probe's self-check values are identical in both
arms. 2097 jit tests, 2610 vm tests and 71/71 regression vectors pass.

### The census is what caught the first version being inert

Bound in the single-pass ladder ALONE, the numbers were: 698 000 native
dispatches with the bind on, 698 000 with it off. Identical. The site counter
said "bound"; the census said "moved nothing". The probe's stores are in a
`main` loop, and **a loop body is an OSR compilation** — which the read bind's
own comment says in as many words about its OSR twin. A timing A/B alone would
have read as noise and been believed.

(A second mistake the same day, for the same file: the OSR block first spliced
INSIDE the read bind's `if let`, immediately after its `continue;` —
unreachable, and it compiled cleanly because the two closing braces landed on
the far side of it. `cargo check` passing proved nothing; reading the nesting
did.)

### What step 1 does NOT buy

`HibfixComposeProbe2` moved **2.5%** (139.8 s -> 136.4 s), and that is the
honest headline for the downstream workload. `tryPushStack` is `NEXT.set(c, h)`
**and** `STACK.compareAndSet(this, h, c)`; only the first is bound, and the CAS
is the more expensive half (489 ns against 303 ns), with more CAS behind it in
`UniCompose.tryFire` and `completeRelay`. Composition is CAS-dominated, so it
stays slow until step 2.

The remaining 64.8 ns is also still 65x HotSpot's 1.0 ns. Two costs are left in
the helper itself, and the second is the bigger prize:

## Steps 2 and 3, in priority order

2. **Bind `compareAndSet`.** This is where the downstream win is. The helper is
   `(vm_ptr, vh, receiver, expected, new)` — five words against Windows' four
   `ARG_REGS`, so unlike `set` it needs the stack-argument setup
   (`emit_stack_arg_setup`, which the direct-call path already has). The store
   half can reuse `vm_exec::compare_and_swap_field`'s logic, which already does
   the hardware CAS with the SATB pre-barrier on `expected` and the post
   `write_barrier` on success.
3. **Stop taking a global mutex per operation.**
   `varhandle_instance_field_plan` locks `vh_meta_table` on EVERY call, on the
   read path as well as this one, and at 24 threads that is a contention point
   before it is a cost. A per-handle memo keyed the way the site key already is
   would take the remaining ~65 ns down and speed the existing read bind up for
   free.

## Repro

```bash
cd apps/hibernate-reactive-suite-runner
java -cp . HibfixVarHandleProbe                      # HotSpot control
cratonvm --java-home <jdk> -Dprobe.iters=2000000 -cp . HibfixVarHandleProbe
```

`HibfixComposeProbe2` is the downstream reproducer — deterministic, no
database, no flake, 412 ms against 359 s.

## Related

- [`../hibernate/hib-reactive-multithreaded-insertion-lazy-connection-20260822.md`](../hibernate/hib-reactive-multithreaded-insertion-lazy-connection-20260822.md)
  §5.9 — where this was found. The reactive composition cost there is this
  defect, and it is the likely enabling condition for that page's correctness
  failure: a sequence-allocation race HotSpot settles in microseconds is run
  through machinery two orders of magnitude slower.
- [`bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md`](bigdecimal-arithmetic-is-50-60x-slower-than-hotspot-20260817.md)
  — the same shape of finding: a hot JDK primitive left on the generic funnel.
