# The native-shadow seal keeps `CompletableFuture` composition INTERPRETED — and that is the 442-872x

## Status
**OPEN (2026-08-24). Root cause identified by a native profile, not fixed.**
The fix is not a two-line guard (see "Why this is not the §6 fix" below).

## Severity
**HIGH.** It is the reason `CompletableFuture` composition measures 442-872x
HotSpot, and it reaches every reactive workload in the tree: the two frames it
seals in Vert.x are the ones in every hibernate-reactive stack trace on the
`MultithreadedInsertionWithLazyConnectionTest` page.

## The measurement that found it

A Java-frame profile put 34% of composition in `CompletableFuture.tryPushStack`
and sent three days of work into `VarHandle` primitives. That reading was
**wrong**, and only a NATIVE profile could say so — a Java-frame sampler
attributes the whole of a native call to the Java frame that made it, so "34% in
tryPushStack" was never evidence about which part of that call was expensive.

`perf record -F 999` on `HibfixComposeProbe2` (Linux, `/data/vhprof`):

| DSO | share |
|---|---:|
| **`cratonvm`** (VM runtime + interpreter) | **92.35%** |
| `libc` | 4.95% |
| **`[JIT]` compiled code** | **0.32%** |

Composition runs almost entirely INTERPRETED. Confirmed independently on the
current Windows binary, where the JIT makes no difference at all:

```
jit:    @@COMPOSE2 threads=2 chains=40000 ms=6142
nojit:  @@COMPOSE2 threads=2 chains=40000 ms=5988
```

`--nojit` is marginally FASTER. There is nothing for the JIT to do because the
code was never compiled.

## Why it is not compiled

`CRATONVM_DBG=jit-method-stats` on the same probe:

```
JIT skip-seal census: 63 method(s) sealed before any compile
  | calls-native-shadowed-method=31 clinit=31 forkjointask-subclass=1
```

A method that calls a JDK method CratonVM has registered a native over is
sealed — permanently, before any compile is attempted
(`interpreter.rs`, `seal_site = "calls-native-shadowed-method"`).
`CompletableFuture.complete` IS such a native
(`native-collections/src/lib.rs`, `r.register(cf, "complete", …)`), and it is
NOT in the `skip_delegating_cf` set that §6 of the hibernate-reactive page
de-registered under a real JDK.

`CRATONVM_DBG_JITC=1` names the casualties. Ignoring `<clinit>` (one-shot,
harmless), from the Vert.x bridge probe:

```
io/vertx/core/Future.lambda$toCompletionStage$5(CompletableFuture, AsyncResult)V
io/vertx/core/impl/future/FutureBase.emitResult(Object, Throwable, Completable)V
java/util/concurrent/CompletableFuture.uniComposeStage(Executor, Function)
java/util/concurrent/ScheduledThreadPoolExecutor.schedule / delayedExecute / triggerTime
java/util/concurrent/AbstractExecutorService.submit(Runnable)
```

The first two are the exact frames in every hibernate-reactive stack trace on
the `MultithreadedInsertionWithLazyConnectionTest` page. The third is the CORE
of `thenCompose`. None of them can ever be compiled.

## Why this is not the §6 fix repeated

§6 de-registered `thenApply`/`thenAccept`/`thenRun`/`thenCompose`/
`exceptionally`/`handle`/`whenComplete` because over a real JDK each was a
**pure delegation** back to the method it shadowed — deleting the native
changed nothing but the funnel.

`native_cf_complete` is not that. It writes `CF_FIELD_RESULT` directly,
substitutes the JDK's `NIL` sentinel for a null value, and distinguishes a
SYNTHETIC CompletableFuture (slot 1 is an Int `done` marker) from a real-JDK one
(slot 1 is the lock-free `stack` reference) by the type of slot 1. Removing it
requires the real JDK's `complete` — and with it `postComplete`, the `stack`
Treiber chain and the `Signaller` path — to work end to end under CratonVM.
That is a project, not a guard.

## What this explains

* **The 442-872x itself.** Every primitive optimised underneath it — the
  `VarHandle` CAS fast path (4 396 000 served, 0.5% on composition), the `set`
  bind, the global mutex — was rearranging furniture inside an interpreter.
* **Why `--nojit` was CLEAN on the hibernate test** (8/8, against 6/8 with the
  JIT) while being no faster: if the reactive path is interpreted either way,
  the JIT's only contribution to that workload is timing VARIANCE, which is
  exactly the shape the cut-loop investigation kept running into.

## What to try next

1. **Measure the blast radius first.** `CRATONVM_DBG=jit-method-stats` prints
   `calls-native-shadowed-method=N` for any workload. Run it on the
   hibernate-reactive suite and on a Spring Boot start before deciding what to
   fix — §6's note that a Spring context seals 856 methods suggests this is not
   a `CompletableFuture` story alone.
2. **Ask whether the seal has to be permanent.** It exists because a caller of a
   shadowed native cannot be compiled correctly. But the `VarHandle` read modes
   ARE served from compiled code today via `VARHANDLE_READ_DIRECT_FNS`, so
   "shadowed" and "uncompilable" are already not the same thing. A native that
   the JIT can emit a direct call to need not seal its callers.
3. Only then consider making `complete` delegate.
