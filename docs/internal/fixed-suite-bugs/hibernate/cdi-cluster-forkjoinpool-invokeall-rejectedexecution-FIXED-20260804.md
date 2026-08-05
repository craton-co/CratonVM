# CDI/Weld cluster (15 classes) — `ForkJoinPool` bulk-submission overloads missing from the real-FJP bridge allow-list

**Status:** FIXED (2026-08-04). All 15 witnesses PASS in the **default**
configuration (no `CRATONVM_SYNTHETIC_FORKJOINPOOL`), with per-class
`found`/`ok` counts identical to HotSpot. The `CRATONVM_SYNTHETIC_FORKJOINPOOL=1`
workaround is no longer needed and should not be used for this cluster.

Fixed on `fix/hib-fjp-invokeall-20260804`.

## What was wrong

`ForkJoinPool.commonPool()` under the (default since 2026-07-30)
`CRATONVM_REAL_FORKJOINPOOL` mode is a VM **Bridge** shortcut: it allocates a
bare `ForkJoinPool` object (`alloc_concurrent_synthetic`, `phases_late/
concurrent.rs`) and never runs the real JDK pool constructor, so the real
`queues`/`runState`/`ctl`/`mode` fields are unset. Every pool method therefore
has to be on an explicit allow-list of VM-implemented bridge methods; anything
**not** listed falls through to real JDK bytecode running against that
under-initialized instance and throws `RejectedExecutionException` at
`submissionQueue()`.

`invokeAll(Collection)` — the overload Weld's `ConcurrentBeanDeployer` →
`AbstractExecutorServices.invokeAllAndCheckForExceptions` actually calls — was
on **neither** allow-list. That is the whole `org.hibernate.orm.test.cdi.*` +
`jpa.cdi.*` cluster plus `org.hibernate.orm.test.filter.FilterParameterTests`.

## The audit found eight more divergences, three of them silent

The original brief asked for an audit of "other missing `ForkJoinPool`/
`ForkJoinTask` overloads". Rather than grep the allow-lists, the audit was
done with a **matrix probe** (`probes/FjpMatrixProbe.java`, 63 rows over the
entire public `ForkJoinPool` surface) run against the **host JDK first** — the
host output is the contract — and then diffed against the CratonVM baseline.
That turned one known bug into nine:

| Row | Baseline | HotSpot |
|---|---|---|
| `invokeAll(Collection)` (+ timed, uninterruptible, singleton) | `RejectedExecutionException` | works |
| `ExecutorService`-typed `invokeAll` (the invokeinterface shape Weld uses) | `RejectedExecutionException` | works |
| `lazySubmit(ForkJoinTask)` | `RejectedExecutionException` | works |
| **`invokeAny(Collection)`** (+ timed) | **returned `null`**, no exception | returns a submitted value |
| **`invokeAny` all-failing** | **returned `null`** | `ExecutionException` |
| **`execute(ForkJoinTask)`** | **`ran=2`** — task body ran TWICE | `ran=1` |
| **`awaitQuiescence` after `execute(Runnable)`** | **`ran=0`** — reported quiescent while the task was still running | `ran=1` |
| `submit(Callable)` whose callable throws | returned `null` | `ExecutionException` |
| `invoke`/`join` on a task whose `compute()` throws | returned `null` | rethrows |
| `isCompletedNormally()` after a throwing `compute()` | `true` | `false` |

The bolded rows are **worse than the reported crash**: they are silent wrong
answers. `invokeAny` ran the callables and then handed back `null`.
`execute(ForkJoinTask)` ran every task body twice, because the bare `exec()`
invoke never RECORDED the task as done, so the matching `join()` found
`done == false` and re-ran it — a correctness bug for any non-idempotent task.

Two of these were found by evidence rather than by reading code:

* **`awaitQuiescence` was in one allow-list but not the other.**
  `--dump-native-registry` showed no `awaitQuiescence` row at all: the entry
  was present in the interpreter's `is_forkjoin_native_override` but missing
  from `keep_real_forkjoinpool_bridge`, so the registration was dropped and the
  interpreter's "force the native" had no native to force. The original brief
  had listed `awaitQuiescence` as already covered. **A name present in only one
  of the two lists is silently inert.**
* **`execute(ForkJoinTask)` had two registrations** (`native-builtins/src/lib.rs`
  and `native-builtins/src/concurrent_extras.rs`). The census named `lib.rs` as
  the one that wins the overwrite; both were fixed so registration order cannot
  decide whether tasks run once or twice.

## The fix

`native-builtins/src/phases_early.rs`:

* `FjpEntry` gains a **`thrown`** slot, rooted by `gc_scan_forkjoin_roots` and
  remapped by `gc_update_forkjoin_refs` exactly like `result`. Every task this
  bridge runs executes inline on the submitting thread, so a throwing
  `call()`/`compute()` arrives as `Err(ExceptionThrown)` from `invoke_virtual`;
  with nowhere to put it the eager-inline natives simply discarded it.
* `fjp_compute_and_complete` (was `fjp_compute_object_result`) records the
  outcome — including an abnormal one — instead of swallowing it. Its
  historical "try `compute()Ljava/lang/Object;`, then `compute()V`, then give
  up with null" retry is now confined to a new `FjtEntry::Unknown` variant for
  task classes with no recognised entry point; replaying that retry for a
  *known* entry point ran a throwing `compute()` a second time.
* `invokeAll` / `invokeAllUninterruptibly` / `invokeAny` (+ timed variants) and
  `lazySubmit` are implemented on the same eager-inline model as
  `submit(Callable)`, plus `ForkJoinTask.getException()`.
* `join()`/`invoke()` replay the task's own throwable unchecked;
  `Future.get()` wraps it in `ExecutionException(cause)`. That wrapper is
  load-bearing rather than cosmetic: Weld's `checkForExceptions` catches
  `ExecutionException` specifically and rethrows its `getCause()`, so a bare
  cause would sail through the catch and a swallowed one would report a
  half-built container as a successful CDI deployment.
* Collection elements are drained into a pinned `FjpBatch` and re-read through
  their own pin handles, because running one callable allocates freely and any
  bare element address would be stale by the next iteration.

Both allow-lists — `is_forkjoin_native_override`
(`vm/src/runtime/interpreter/native_override.rs`) and
`keep_real_forkjoinpool_bridge` (`native-api/src/registry.rs`) — received every
new `(name, descriptor)` pair, plus the missing `awaitQuiescence` entry.

## Verification

Three arms, same classpath and same `@common.args`, differing only in the VM.

**Matrix probe** (`probes/FjpMatrixProbe.java`, 63 rows, diffed against the
host JDK):

```
baseline divergent : 26
fixed    divergent : 4
```

The probe is run twice on HotSpot and the two runs are diffed against each
other first, so a row that is merely nondeterministic cannot be mistaken for a
CratonVM divergence (0 flaky rows). One row had to be rewritten for this:
`invokeAny` returns whichever callable wins the race, and HotSpot genuinely
returns a different one run to run, so the row asserts "the result is one of
the submitted values" rather than naming a value the JDK leaves unspecified.

The 4 residuals are not regressions:

* `commonPool.identity` — `commonPool()` mints a fresh carrier per call instead
  of a singleton. **Pre-existing**, unrelated to this bug, and already recorded
  as a `TODO` at the `commonPool` registration. Not reached by Weld
  (`CommonForkJoinPoolExecutorServices.cleanup()` is a no-op, so the pool is
  never compared or shut down).
* Three exception **message** differences, class-identical. HotSpot's
  `ForkJoinTask.getThrowableException()` reconstructs the exception when the
  joining thread differs from the recording thread, yielding a cause whose
  message is the original's `toString()` (`"java.lang.IllegalStateException:
  boom"`). This model runs inline on the caller, so the **original** exception
  is delivered (`"boom"`). Same cause class; `cause instanceof RuntimeException`
  — what Weld actually branches on — behaves identically.

**The 15 witnesses** (default config, no `CRATONVM_SYNTHETIC_FORKJOINPOOL`):

| arm | result |
|---|---|
| HotSpot (control) | 15/15 PASS |
| CratonVM baseline (`origin/dev`, `41349f661`) | **15/15 FAIL** |
| CratonVM fixed | **15/15 PASS**, per-class `found`/`ok` identical to HotSpot |

`RejectedExecutionException` appears in 15/15 baseline logs and **0/15** fixed
logs. `FilterParameterTests` reproduces the brief's addendum exactly on the
baseline (`found=10 ok=6 failed=4`) and reaches `found=10 ok=10` fixed.

**Regression fixture:** `vm/tests/resources/cratonvm/RealFjp.java` +
`real_fjp_path` in `vm/tests/synthetic_diff.rs`. It asserts PAIRED properties —
the futures' size AND their results AND that the callables *actually ran* AND
that the futures report done AND that a throwing callable surfaces
`ExecutionException` — so a bridge that silently dropped the work cannot score
a clean pass. The fixture was confirmed to be a real guard by running it
against the baseline binary, where it fails with the brief's exact frames:

```
java/util/concurrent/RejectedExecutionException
    at java/util/concurrent/ForkJoinPool.invokeAll(ForkJoinPool.java:3389)
    at java/util/concurrent/ForkJoinPool.invokeAll(ForkJoinPool.java:3373)
```

Note that `vm/tests/resources/**/*.class` are **committed fixtures** — editing
the `.java` alone silently keeps testing the stale class file.

## Why it appeared on 2026-07-30

`16ec5d7ad` ("wip: deep-audit agent handoff snapshot") flipped
`real_forkjoinpool` from opt-in to default-on
(`!present("CRATONVM_SYNTHETIC_FORKJOINPOOL") || present("CRATONVM_REAL_FORKJOINPOOL")`).
`vm_init.rs`'s `org.jboss.weld.executor.threadPoolType=NONE` seed — `HIB-CV-20`'s
"Layer 2" fix, which kept Weld on the single-threaded `SimpleBeanDeployer` and
away from `ForkJoinPool` entirely — is conditioned on
`!flags().natives.real_forkjoinpool`, so it stopped firing by default at the
same moment. That routed every CDI class through `ConcurrentBeanDeployer` →
`invokeAll` → the uncovered overload.

## Superseded corrections

The brief's own "Corrections to existing docs" section added 2026-08-04
corrections to three previously-closed docs whose PASS claims this bug had
invalidated. All three are now re-verified against the fixed binary:

* [`HIB-CV-20`](HIB-CV-20-weld-observer-beanmanager-param-reflection.md) — its
  Layers 1/2/3 fixes were never wrong; the regression was this separate
  coverage gap. Real-FJP-by-default now works without the
  `threadPoolType=NONE` fallback.
* [`HIB-CV-25`](HIB-CV-25-cdi-weld-qualifier-containsall-foreign-collection.md)
  — claimed "9/11 sampled `cdi.*` PASS == HotSpot". Re-verified: **14/14**
  `cdi.*`/`jpa.cdi.*` PASS == HotSpot.
* [`HIB-CV-25b`](HIB-CV-25b-weld-clientproxy-dataoutputstream-written-slot.md)
  — claimed "all 12 sampled `cdi.*` PASS == HotSpot". Re-verified: **14/14**.

[`hib-delayedcdisupporttest-weld-bootstrap-hang-NOT-A-BUG.md`](hib-delayedcdisupporttest-weld-bootstrap-hang-NOT-A-BUG.md)
was correctly left uncorrected — its subject is a *hang*, orthogonal to this
fast deterministic failure.

## Not tested

`--nojit`: not run. The failure is entirely in Weld/JDK bootstrap bytecode
before any hot loop, and the fix is a native-registration/allow-list change
with no JIT-visible surface. HotSpot is unaffected by construction —
`commonPool()` on a real JDK is a fully-initialized real pool.
