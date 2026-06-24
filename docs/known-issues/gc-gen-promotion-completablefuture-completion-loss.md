# `CompletableFuture` untimed `get()`/`join()` never wakes on cross-thread completion — FIXED

**Status:** ✅ **FIXED.** The fix is in the synthetic `CompletableFuture.complete` /
`completeExceptionally` natives (`native-collections/src/lib.rs`,
`native-builtins/src/lib.rs`). No GC change was needed.

> ⚠️ **The original diagnosis in this file was WRONG.** It attributed the hang to
> the generational (copying) GC losing a young `Signaller` when the future is
> promoted to old gen, and claimed `-XX:+UseG1GC` and `CRATONVM_NO_GC_PROMOTION=1`
> were immune/reliable fixes. **Re-measured: the hang is 100 % deterministic and
> GC-independent** — `default gen-GC`, `-XX:+UseG1GC`, `CRATONVM_NO_GC_PROMOTION=1`,
> and `--nojit` **all hang 6/6**. The earlier "G1/NO_GC_PROMOTION pass" runs were
> flaky luck, not immunity. The real cause is a native-override gap (below). This
> file is kept (and the wrong theory called out) so the dead-end isn't re-walked.

## Symptom

A thread blocked in untimed `CompletableFuture.get()` / `join()` (which funnel
through `waitingGet()`) is **never woken** when another thread calls `complete()`
(or `completeExceptionally()`) *after* the waiter has parked. `complete()` returns
normally; the waiter hangs forever. The **timed** `get(timeout, unit)` works
because its `parkNanos` loop re-polls `result` and "self-heals". This blocks every
async-HTTP-client pattern (Spring's Jetty/Jdk/Reactor/HttpComponents
`ClientHttpRequestFactoryTests` all `TIMEOUT`, since the blocking `send()` ==
`CompletableFuture.get()`), and the Keycloak/Quarkus boot's `JPAConfig.startAll()`.

Minimal deterministic repro:

```java
var f = new CompletableFuture<String>();
new Thread(() -> { Thread.sleep(500); f.complete("x"); }).start();
f.get();   // HANGS (rc=124) — get(timeout) returns fine
```

## Root cause (confirmed)

`java.util.concurrent.CompletableFuture.complete(Object)Z` is registered as a
**synthetic native** (`native-collections/src/lib.rs::native_cf_complete`, plus
shadows in `native-builtins`) that win over the real-JDK bytecode via the
native-override path in `try_stackless_invoke`
(`vm/src/runtime/interpreter.rs`, the `native_methods.find(class, method, desc)`
arm fires *before* the has-own-bytecode check). The synthetic native only stored
the `result` field and **never ran `postComplete()`**.

For a **real-JDK** `CompletableFuture` (allocated by the genuine bytecode `<init>`;
layout = `result`@0 + the lock-free `stack`@1), the untimed `waitingGet()` pushes a
`Signaller` onto `stack`@1 and parks via `LockSupport.park`. `complete()` is
supposed to run `postComplete()`, which pops the `stack` and fires each `Signaller`
(`thread = null; LockSupport.unpark(waiter)`). The synthetic native skipped that
entirely, so the parked thread's `unpark` was **never issued** — an indefinite
hang on every cross-thread completion. (It also wrote the synthetic `done` Int into
slot 1, clobbering the real `stack` reference, but the missing `unpark` is the
direct cause of the hang.)

`get()` itself runs real bytecode (its native didn't win), so the waiter side was
always correct; only the completer side dropped the wakeup. That asymmetry — real
bytecode parks, synthetic native completes-without-`postComplete` — is the whole
bug.

## Fix

The synthetic `complete` / `completeExceptionally` natives now detect a real-JDK
`CompletableFuture` and run the genuine completion path so waiters are unparked:

- **Discriminator:** slot 1's *value type*. A synthetic CF carries an Int `done`
  flag there; a real-JDK CF's `stack` is always a reference (null or a
  `Completion`/`Signaller` chain). The object's field **count is NOT usable** — a
  real-JDK CF is allocated with ≥4 slots here, so the old `num_fields >= 4` check
  misclassified it as synthetic.
- **Real-JDK path:** after storing the result (`complete`) /
  `obtrudeException` (`completeExceptionally`), invoke the real
  `postComplete()` (`ctx.invoke_virtual(this, "postComplete", "()V", &[])`) to pop
  the `stack` and `LockSupport.unpark` every parked waiter.
- **Synthetic path:** unchanged (`done` flag model; no waiter stack).

## Regression witnesses (all PASS, JIT + `--nojit` + G1)

- untimed `get()` cross-thread `complete()` (race window 0 ms … 2000 ms);
- untimed `get()` cross-thread `completeExceptionally()` → `ExecutionException`;
- `completedFuture` / `supplyAsync` / `thenApply` / pre-completed `get()` / double
  `complete()` (synthetic-model paths unaffected);
- `ForkJoinPool.managedBlock`, `LockSupport.park`/`unpark`, and the cross-thread
  Treiber-stack CAS probe.

## Related

- [keycloak-quarkus-boot-progress.md](keycloak-quarkus-boot-progress.md) — the
  `JPAConfig.startAll()` boot hang attributed here to GC was this same bug.
- The Jetty NIO client work (`DirectByteBuffer.put(byte)` + NIO connect-probe) that
  surfaced this as the last async-HTTP-client blocker.
