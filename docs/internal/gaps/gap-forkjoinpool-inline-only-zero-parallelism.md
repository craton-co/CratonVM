# `ForkJoinPool` never spawns worker threads — every task runs inline, zero parallelism

## Status
**OPEN** (2026-08-23). Root cause already understood and documented as a
deliberate architectural tradeoff (see "Root cause" below) — this doc exists
to record the *consequence* for `java.util.stream` parallel operations as its
own tracked, scoped defect, separate from the specific hang this
architecture was built to avoid.

## Severity
**MEDIUM-HIGH, broad blast radius.** Not a correctness bug — every case
tested produces the right answer. But `parallelStream()` / `IntStream.parallel()`
/ explicit `ForkJoinPool` usage get **zero speedup, ever, on any hardware**,
and pay roughly 40% overhead versus an equivalent plain sequential loop for
CPU-bound work. `parallelStream()` is one of the most common "make this
faster" idioms in the Java corpus this project benchmarks against (Spring,
Hibernate, and plenty of ordinary application code reach for it by default),
so this is silently costing performance broadly, not in one workload.

## Symptom

Any of: `Collection.parallelStream()`, `IntStream.range(...).parallel()`
(or `Stream`/`LongStream`/`DoubleStream` equivalents), any terminal
operation on such a stream (`forEach`, `collect` — both a plain accumulator
collector and a combiner-based reduction collector were checked), an
explicit `ForkJoinPool.submit()`/`invoke()`/`execute()`, and the static
`ForkJoinTask.invokeAll` overloads — all execute their work **entirely on
the calling thread**. No additional worker thread is ever created,
regardless of the pool's configured or reported parallelism level.

Found while benching the real (unsimplified) `TornadoVM-Ray-Tracer` app's
`Renderer.renderWithParallelStreams` — a nested nested `IntStream.range(...)
.parallel().forEach(...)` over image columns then rows. On a 32-logical-core
box:

| | HotSpot | CratonVM |
|---|---:|---:|
| Sequential render | 225.9ms | 23,457.3ms |
| Parallel-streams render | 33.0ms (**~7x faster**) | 31,187.9ms (**slower than sequential**) |

That ratio (parallel slower than sequential, on the same VM) was the tell
that something structural was wrong, not ordinary JIT/allocation overhead —
isolated from there with minimal, app-independent repros.

## Confirmed scope — precisely which mechanisms are affected

Every repro below was run as a matched HotSpot/CratonVM pair on the same
32-logical-core box, same binary (`target-gpuray/release/cratonvm-gpuray.exe`,
current `dev` @ 2026-08-22).

**Affected — inline-only, zero parallelism:**

| Mechanism | HotSpot | CratonVM |
|---|---|---|
| `ForkJoinPool.commonPool().getParallelism()` | 31 | **1** |
| distinct worker threads for a 2M-element `.parallel().forEach()` | 31 (+ `main`) | **1** (`main` only) |
| same, elapsed | 325ms | 157,227ms |
| same work, plain sequential loop | 4,001ms | 112,233ms (**"parallel" is 1.4x *slower* than sequential**) |
| explicit `new ForkJoinPool(8)`: `.getParallelism()` reported | 8 | 8 (**correct — see note below**) |
| same pool, actual distinct threads used | 8+ | **1** (`main` only — despite correct metadata) |
| `.collect(Collectors.toList())` on a parallel stream | many threads | **1** (`main`) |
| `.collect(Collectors.summingLong(...))` (combiner path) on a parallel stream | many threads | **1** (`main`) |

The explicit-pool row is the most informative single data point:
`getParallelism()` is just a stored config value that doesn't require a
worker thread to exist to report correctly, so an explicit pool's metadata
looks right while its execution is exactly as single-threaded as the common
pool's. This rules out "only the common pool's default computation is
wrong" — the defect is that **no `ForkJoinPool`, common or explicit, ever
spawns additional worker threads**; everything submitted runs synchronously
on the submitting thread.

**Not affected — genuinely concurrent, verified separately:**

| Mechanism | HotSpot | CratonVM |
|---|---|---|
| `CompletableFuture.supplyAsync`/`runAsync` (default executor) | real async, `ForkJoinPool.commonPool-worker-N` | real async, `pool-1-thread-N` (a **different**, working pool — not routed through the broken common-pool path) |
| 8x independent 500ms `runAsync` tasks, wall time | 507ms | 652ms (real concurrency) |
| reentrant case: 32 tasks running *on* the pool, each submitting + `.join()`-blocking on 1 more task on the *same* pool | 4ms (managed-blocking compensation) | 667ms — **no deadlock**, correct result |
| `Thread.ofVirtual()` / `Executors.newVirtualThreadPerTaskExecutor()` | real concurrency | real concurrency (dedicated carrier-thread subsystem, unrelated code path) |
| 32 concurrent 500ms virtual threads, wall time | 515ms | 520ms |
| same reentrant-submission shape via virtual-thread executor | 510ms | 528ms — no deadlock |

So this is scoped tightly to `ForkJoinPool`/fork-join-task mechanisms
specifically — not a blanket "concurrency is broken" situation.
`CompletableFuture`-based async code and virtual threads both get real
parallelism today.

## Root cause (already documented, cross-referenced here)

Fully explained in
[`docs/internal/fixed-bugs/jdk-only-L12-forkjoinpool-no-workers-awaitdone-hang-FIXED-20260811.md`](../fixed-bugs/jdk-only-L12-forkjoinpool-no-workers-awaitdone-hang-FIXED-20260811.md),
which fixed a **hang** caused by the same underlying design (a `fork()` +
`awaitDone()` pair where nothing ever drives the forked sibling) and
explicitly predicted-but-did-not-verify that `parallelStreams()` would show
this exact zero-parallelism signature — this doc, and the 2026-08-23
addendum on that one, is that verification.

In short: `ForkJoinTask.fork()` is a deliberately lazy Bridge native that
only marks a task queued in a side table; what actually *drives* `compute()`
is `join()`/`get()`/`invoke()`, each of which runs the task body inline on
whichever thread calls it and memoises the result. `ForkJoinPool.invoke()`/
`submit()`/`execute()` are the same shape: they run the submitted body on
the caller and never hand anything to a separate worker. This exists
because CratonVM's `NativeContext` is not `Send` — worker OS threads cannot
run Java bytecode in this VM's current design — so the whole `ForkJoinTask`
family is implemented as an eager-inline emulation on top of that
constraint, not real fork/join scheduling. That is a considered
architectural tradeoff for the specific class of hang it closes, not an
oversight; **this doc is not asking to reopen that call** — it exists to
give the resulting "parallel streams do nothing" performance
characteristic its own tracked, named identity, since it wasn't verified or
scoped until now.

## Why `CompletableFuture` and virtual threads are unaffected

Neither goes through the `ForkJoinTask`/`ForkJoinPool` Bridge path at all.
`CompletableFuture`'s default async executor resolves to a separate,
CratonVM-specific thread-pool mechanism (`pool-1-thread-N` naming, distinct
from both `main` and `ForkJoinPool.commonPool-worker-N`) that genuinely
spawns OS threads. Virtual threads are an entirely different subsystem
(`vm/src/threading/virtual_threads.rs`, dedicated `VirtualThreadManager`
carrier pool) that predates and is architecturally unrelated to the
`ForkJoinTask` inline-emulation model.

## Fix directions (not attempted here — out of scope for this triage)

1. **Narrow fix, most promising:** route `java.util.stream`'s parallel
   data-parallel case through the same mechanism `CompletableFuture` already
   uses successfully, rather than through the general `ForkJoinTask`
   inline-emulation model. Streams' `AbstractTask`/`CountedCompleter`-based
   splitting doesn't fundamentally need `ForkJoinPool` specifically — it
   needs *some* pool of threads that can each independently execute
   bytecode, which the `CompletableFuture` pool already demonstrates is
   achievable under CratonVM's current `NativeContext` constraints.
2. **General fix, large:** make `NativeContext` `Send` so real
   `ForkJoinWorkerThread`s can run Java bytecode, enabling genuine
   multi-threaded fork/join scheduling. This is the redesign the L12 doc
   already flagged as out-of-scope for a single lane — it would change
   `fork()` ordering/semantics for every Spring/Hibernate/JUnit/stream
   workload in the corpus and wants its own dedicated effort with an A/B,
   not a quick patch.
3. Whatever direction is chosen, re-run this doc's repros (below) as the
   acceptance check — they're minimal and fast (a few seconds each on
   HotSpot; the CratonVM baseline numbers above are the "before").

## Repro

Minimal, self-contained, no app dependency:

```java
import java.util.concurrent.ForkJoinPool;
import java.util.concurrent.ConcurrentHashMap;
import java.util.Set;
import java.util.stream.IntStream;

public class ParStreamDiag {
    public static void main(String[] args) throws Exception {
        System.out.println("availableProcessors=" + Runtime.getRuntime().availableProcessors());
        ForkJoinPool cp = ForkJoinPool.commonPool();
        System.out.println("commonPool.getParallelism=" + cp.getParallelism());

        Set<String> threadNames = ConcurrentHashMap.newKeySet();
        long t0 = System.nanoTime();
        IntStream.range(0, 2_000_000).parallel().forEach(i -> {
            threadNames.add(Thread.currentThread().getName());
            double x = i;
            for (int k = 0; k < 50; k++) x = Math.sin(x) * Math.cos(x) + Math.sqrt(Math.abs(x));
        });
        System.out.println("distinct_worker_threads=" + threadNames.size()
                + " elapsed_ms=" + (System.nanoTime() - t0) / 1_000_000.0);
    }
}
```

Expected on a fixed build: `distinct_worker_threads` > 1, matching
`commonPool.getParallelism` roughly, and `elapsed_ms` in the same ballpark
as HotSpot's (a few hundred ms on a 32-core box), not tens of seconds.

## Related files

- `docs/internal/fixed-bugs/jdk-only-L12-forkjoinpool-no-workers-awaitdone-hang-FIXED-20260811.md` — root cause, and the 2026-08-23 addendum with the full set of confirming/scoping measurements this doc summarizes
- `docs/internal/arch-2026-07-26/virtual-threads.md` — the separate, working virtual-thread carrier subsystem
- `native-builtins/src/phases_early.rs` (`register_real_jdk_forkjoin_essentials`, `register_forkjoin_natives`) — the lazy `fork()` Bridge
- `native-builtins/src/phases_late/concurrent.rs` (`register_forkjointask_invoke_all_bridge`) — the L12 fix
- `apps/TornadoVM-Ray-Tracer/src/main/java/com/vinhderful/raytracer/renderer/Renderer.java` — the real-world workload that surfaced this (`renderWithParallelStreams`, line ~172)
