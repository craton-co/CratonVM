# W6-9 — `ForkJoinTask.complete(v)` erased the abnormal record, and
# `completeExceptionally` was never registered at all

Status: **fix written (unbuilt, unmeasured)**. Wave 6, lane W6-9. Takes the
defect W6-7 found and declined ("live long-standing registrations, blast radius
unmeasured").

Predecessors: `W2-8-forkjointask-invoke-returns-computes-null.md`,
`W3-4-forkjointask-status-flags-and-the-eager-default.md`,
`W6-7-forkjointask-quietly-family.md`.

---

## 1. The real contract, from the image

`javap -p -c java.util.concurrent.ForkJoinTask` (JDK 25.0.3.9). Status bits, read
off the `ldc` constants:

| constant | value | bits |
|---|---|---|
| `DONE` | `-2147483648` = `0x80000000` | `1<<31` |
| `ABNORMAL` | `65536` = `0x00010000` | `1<<16` |
| `DONE\|ABNORMAL` | `-2147418112` = `0x80010000` | `trySetCancelled` ORs this |
| `DONE\|ABNORMAL\|THROWN` | `-2147287040` = `0x80030000` | `trySetThrown` ORs this |

```
private void setDone();
   0: aload_0
   1: ldc           int -2147483648      // DONE
   3: invokevirtual getAndBitwiseOrStatus:(I)I
   6: pop
   7: aload_0
   8: invokevirtual signalWaiters:()V
  11: return
```

**`setDone()` is a bitwise OR and nothing else. It cannot clear `ABNORMAL`.**
Neither can `trySetCancelled` or `trySetThrown` — all three are OR-into-a-
write-once-word. The only method in the class that ever *clears* a status bit is
`reinitialize()`:

```
public void reinitialize();
   0: aload_0; 1: aconst_null; 2: putfield  aux
   5: aload_0; 6: dup; 7: getfield status:I
  10: ldc       int 16777216             // 1<<24, the pool-submit bit
  12: iand
  13: putfield  status:I
  16: return
```

`complete(V)` is `setRawResult` then `setDone`, with the raw-result write
guarded:

```
public void complete(V);
   0: aload_0; 1: aload_1
   2: invokevirtual setRawResult:(Ljava/lang/Object;)V
   5: goto 15
   8: astore_2                            // catch Throwable
   9: aload_0; 10: aload_2
  11: invokevirtual trySetException:(Ljava/lang/Throwable;)V
  14: return
  15: aload_0
  16: invokevirtual setDone:()V
  19: return
  Exception table:  from 0 to 5 target 8  Class java/lang/Throwable
```

`completeExceptionally` wraps a checked throwable before recording it:

```
public void completeExceptionally(java.lang.Throwable);
   ... ex instanceof RuntimeException || ex instanceof Error
       ? ex : new RuntimeException(ex)
  27: invokevirtual trySetException:(Ljava/lang/Throwable;)V
```

and `trySetException` -> `trySetThrown`, which requires `status >= 0` (`iflt 125`
at offset 11) — i.e. **the first completion wins; a later one is a no-op**.

`CountedCompleter.complete(T)` (`javap -p -c java.util.concurrent.CountedCompleter`)
is a different method and correctly reaches different code:

```
public void complete(T);
   2: invokevirtual setRawResult:(Ljava/lang/Object;)V
   7: invokevirtual onCompletion:(Ljava/util/concurrent/CountedCompleter;)V
  11: invokevirtual quietlyComplete:()V
  24: invokevirtual tryComplete:()V      // on the completer, if non-null
```

`tryComplete()` and `propagateCompletion()` both end in `quietlyComplete()`
(offsets 30 and 23), which is the bare `setDone()` W6-7 already routed to
`fjp_state_set_done_preserving_thrown`.

**Answer to the question this lane was asked to settle: no, `setDone` cannot
clear `ABNORMAL`, and yes, `complete(T)` writes the raw result through a separate
virtual `setRawResult`.** W6-7's reading was right.

## 2. What CratonVM did

`fjp_state_set_done` (`native-builtins/src/phases_early.rs`) ended with

```rust
e.done = true;
e.result = result;
e.thrown = Value::Object(None);   // <-- erases the abnormal record
```

and all four `complete(Ljava/lang/Object;)V` registrations were a bare
`fjp_state_set_done(this, val)`. So on a task that had already completed
exceptionally, `t.complete(v)`:

* cleared `thrown`, so `isCompletedAbnormally()` -> `false`,
  `isCompletedNormally()` -> `true`, `getException()` -> `null`;
* made `join()`/`get()` hand back `v` instead of replaying the throwable.

Real JDK: all four of those answers are the opposite, because `setDone()` ORs one
bit into a word that already has `ABNORMAL|THROWN` set. This is the campaign's
dominant species — **a fabricated success where the spec mandates a failure** —
on the exact trio W3-4 built to remove it.

A second, independent instance was found in the same audit: **`completeExceptionally
(Ljava/lang/Throwable;)V` was registered nowhere and named in neither allow-list**
(`grep -c '"completeExceptionally"'` was `0` on both). In real-JDK mode it ran
real bytecode, which CASes the real `aux`/`status` fields — which nothing in this
VM reads, because every completion this model publishes lives in the `fjp_state`
side table. So a task the caller had explicitly failed stayed `done == false`,
and the **next `join()` ran its body and returned a value**. Not a hang: a silent
wrong answer, and the reason the falsifier below could not even be stated against
the tree as it stood.

## 3. The fix

1. `fjp_state_set_done` no longer touches `thrown`. It is now exactly
   `setRawResult(v); setDone();`.
2. A shared `fjp_complete_body` behind all four `complete` registrations. Besides
   the preserved `thrown`, it invokes the **virtual** `setRawResult` for any
   receiver whose raw-result slot is its own field rather than the side table
   (`fjt_has_own_raw_result_slot`: not one of `ForkJoinTask`/`RecursiveTask`/
   `RecursiveAction`, and `method_exists` says the method is there). Those three
   classes keep their raw result in `FjpEntry::result` — their `getRawResult`/
   `setRawResult` registrations read and write it — so calling the virtual would
   only re-enter this module; every other receiver (a user subclass,
   `java.util.stream.AbstractTask`) had its field left untouched and answered
   null from a later real `getRawResult()`. A `setRawResult` that throws is
   recorded as the task's abnormal completion, matching the real
   `catch (Throwable rex) { trySetException(rex); }`.
3. `completeExceptionally(Ljava/lang/Throwable;)V` registered on all three task
   classes in both boot paths, with the real wrapping rule and the real
   `status >= 0` write-once guard. **One new triple, so both allow-lists gain one
   entry** (see §5).

### Should `fjp_state_set_done` keep an erasing variant at all?

No. Argued from the callers, which are the complete set (`grep -n
'fjp_state_set_done'`):

| caller | wants the erasure? |
|---|---|
| `fjp_complete_from_outcome`, `Ok(value)` arm | **No** — unreachable with `thrown` set. Every entry into `fjp_compute_and_complete` is gated on `!done` (`fjp_join_body`, `fjp_future_get_body`, `fjp_quietly_body`, `pool.invoke`, `fjp_compute_for_submit`), and `fjp_state_set_thrown` always sets `done`. The erasure was a no-op here. |
| `ForkJoinPool.shutdown` / `shutdownNow` (x2) | **No** — pool keys, never a task; `thrown` is never set on one. |
| the four `complete(Object)V` registrations | **No** — this was the defect. |
| `fjp_gc_tests::gc_hooks_scan_result_and_remap_key_and_result` | **No** — asserts key/result remapping only. |

The one API that legitimately wants a reset is `reinitialize()` (see the
disassembly in §1: it clears `DONE`/`ABNORMAL`/`THROWN` and nulls `aux`).
**`reinitialize` is registered nowhere** — `grep -rn '"reinitialize"'
--include=*.rs .` is empty — so it is not a caller of this primitive and cannot
become one without its own lane. That is what makes deleting the erasure safe
rather than merely convenient: there is no reuse path in the tree that needs it.

## 4. Blast radius

`grep -rn '\.complete(\|completeExceptionally' --include=*.java .` over the whole
tree (probes, `regression-suite/src/`, `vm/tests/`, `apps/`, `docs/known-issues/
repros/`) returns **five** hits, all in `vm/tests/resources/cratonvm/JucComplete.java`,
and all on `CompletableFuture` — class `java/util/concurrent/CompletableFuture`,
descriptor `(Ljava/lang/Object;)Z` / `(Ljava/lang/Throwable;)Z`, a different
registration in `phases_late/concurrent.rs` that this lane does not touch.

**There is no caller of `ForkJoinTask.complete(Object)` anywhere in the tree.**
The only ForkJoin completion call in Java sources is `RJdkForkJoin.java:154,160`
— `CountedCompleter.tryComplete()`, which resolves to `CountedCompleter`'s own
bytecode (that class is in neither allow-list, deliberately, per W6-7 §"NOT
registered") and reaches `quietlyComplete()`, already the preserving Bridge. Its
assertions (`leaves.get() == 64`, the `completions` count) read pending-count
propagation, not `thrown`.

The three Java assertions that read the abnormal record —
`RJdkForkJoin.java:259-261` (`isCompletedAbnormally` / `!isCompletedNormally` /
`getException() instanceof IllegalStateException`) and `:294` (cancelled flags),
plus `probes/FjpMatrixProbe.java:167-182` — all reach it through a **throwing
`compute()`**, i.e. `fjp_state_set_thrown` from `fjp_complete_from_outcome`.
None calls `complete()`, so none was observing the erasure and none changes.

No Rust test asserts on it either: `vm/tests/fjp_recursive.rs` and
`vm/tests/rfjp1_recursive.rs` contain no `complete`/`getException`/`Abnormal`
match.

So: **no caller in the tree depends on the erasing behaviour.** That is the
measurement W6-7 was missing.

## 5. The three-edit rule

`complete(Ljava/lang/Object;)V` was already on both allow-lists, so the
`complete` half needs no allow-list change. `completeExceptionally` is a new
triple and needs both:

* `native-api/src/registry.rs`, `keep_real_forkjointask_bridge`
* `vm/src/runtime/interpreter/native_override.rs`, `is_forkjoin_native_override`

Add `| ("completeExceptionally", "(Ljava/lang/Throwable;)V")` immediately after
the `("getException", "()Ljava/lang/Throwable;")` entry in each. Expected counts
after the patch:

```
grep -c '"completeExceptionally"' native-api/src/registry.rs                    -> 1
grep -c '"completeExceptionally"' vm/src/runtime/interpreter/native_override.rs -> 1
grep -c '"completeExceptionally"' native-builtins/src/phases_early.rs           -> 4
```

(4 = `fjt` + `rt` + `ra` in the synthetic `register_forkjoin_natives`, plus one
literal inside the three-class loop in `register_real_jdk_forkjoin_essentials`.)
Before the patch the first two were **0** — the registration would have been
silently dead in real-JDK mode.

## 6. Falsifier

```java
ForkJoinTask<Integer> t = new RecursiveTask<>() {
    protected Integer compute() { return 1; }
};
t.completeExceptionally(new IllegalStateException("boom"));
t.complete(99);
check(t.isCompletedAbnormally(),  "still abnormal after complete()");
check(!t.isCompletedNormally(),   "not normal after complete()");
check(t.getException() instanceof IllegalStateException, "ISE survives complete()");
```

Before: `isCompletedAbnormally()` false and `getException()` null — for two
independent reasons (the erasure, and `completeExceptionally` never reaching the
side table). After: true / false / ISE. `t.join()` must still throw the ISE.

## 7. Known divergences left open (NOT fixed here)

* **`getException()` on a cancelled task answers `null`.** The real
  `getException(boolean)` returns a fresh `CancellationException` when the task
  is `ABNORMAL` with no recorded throwable. Today `isCompletedAbnormally()` says
  `true` for a cancelled task while `getException()` says "nothing went wrong" —
  the two contradict each other. Fixing it means allocating a
  `CancellationException` (or the `IllegalStateException` proxy
  `fjp_state_get_checked` already uses) from inside `getException`, which changes
  behaviour for every cancelled task; it needs its own measurement.
* **`reinitialize()` is unregistered.** In real-JDK mode it resets the real
  `status`/`aux`, which this model never reads, so a reused task keeps its
  side-table completion and the next `join()` returns the stale result instead of
  recomputing. Latent: no caller in the tree.
* **`RecursiveAction.complete(Object)` is not registered** (only `ForkJoinTask`
  and `RecursiveTask` are, in both boot paths). In real-JDK mode the resolved
  declaring class for an `ra` receiver is `ForkJoinTask`, so the `fjt`
  registration covers it; in synthetic mode there is a hole. Left as found —
  adding it is a 5th registration this lane has no evidence for.
* **`complete(v)` on a cancelled task is a full no-op here**, where the real
  `complete` still runs `setRawResult(v)` and only the status write is
  suppressed. Diverges on a bare `getRawResult()` alone; `join()`/`get()` raise
  `CancellationException` either way. Deliberately left: changing it would also
  change what `ForkJoinPool.invoke` hands back for a cancelled task, which is a
  separate (larger) divergence.
