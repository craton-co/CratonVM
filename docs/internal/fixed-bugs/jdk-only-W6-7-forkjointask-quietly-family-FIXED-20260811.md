> **FIXED 2026-08-11 — moved out of `docs/known-issues/jdk-only/`.**
>
> Vector `RJdkForkJoin` passes in the 53/1 run. The `quietly*` registrations and the `quietlyComplete()` semantics are in `native-builtins/src/phases_late/concurrent.rs`. The one item under "Recorded, not fixed" — `complete(Ljava/lang/Object;)V` erasing the abnormal record — was taken by W6-9, which stays open in `docs/known-issues/jdk-only/` with a fix in flight.
>
> Previous location: `docs/known-issues/jdk-only/W6-7-forkjointask-quietly-family.md`.
> Audit that moved it: `docs/known-issues/jdk-only/RETIREMENT-20260811.md`.

# W6-7 — the `ForkJoinTask.quietly*` family was registered nowhere, so every
# member ran real bytecode into `awaitDone()`

Status: **fix written (unbuilt, unmeasured)**. Wave 6, lane W6-7. Closes §4 of
`W3-4-forkjointask-status-flags-and-the-eager-default.md`, which specified this
work and deliberately deferred it.

Predecessors: `L12-forkjoinpool-no-workers-awaitdone-hang.md`,
`L19-countedcompleter-lazy-fork-starvation.md`,
`W2-8-forkjointask-invoke-returns-computes-null.md`,
`W3-4-forkjointask-status-flags-and-the-eager-default.md`.

---

## 1. The defect

CratonVM runs **no ForkJoin worker threads** — `NativeContext` is not `Send`, so
there is no thread to hand a forked task to. Every task is computed inline on
the calling thread and completed in the `fjp_state` side table
(`native-builtins/src/phases_early.rs`); the real `ForkJoinTask.status` field is
never written.

Disassembled from the image (`javap -p -c java.util.concurrent.ForkJoinTask`,
JDK 25.0.3.9):

```
public final void quietlyJoin();
   0: aload_0
   1: getfield  status:I
   4: iflt      14
   7: aload_0; 8: iconst_0; 9: lconst_0
  10: invokevirtual awaitDone:(ZJ)I
  13: pop
  14: return

public final void quietlyInvoke();
   0: aload_0
   1: invokevirtual doExec:()V
   4: aload_0
   5: getfield  status:I
   8: iflt      18
  11: aload_0; 12: iconst_0; 13: lconst_0
  14: invokevirtual awaitDone:(ZJ)I
  17: pop
  18: return

public final void quietlyComplete();
   0: aload_0
   1: invokevirtual setDone:()V
   4: return
```

`status` is 0 for every task this VM completes, so `iflt` falls through and the
call enters `awaitDone` — which blocks until a completion nothing will ever
publish. That is the **300-second hang** L12 fixed for the static `invokeAll`
overloads, arriving one method later. It is not a wrong answer; it is a stop.

`grep -rn '"quietly' --include=*.rs .` returned **nothing** before this lane: all
six were registered nowhere and named in neither allow-list.

### Latent, not live — and why it still had to be fixed

Wave 2 disassembled `ReduceOps` / `ForEachOps` / `MatchOps` / `FindOps` /
`Nodes` and measured **17 `invoke()` call sites and ZERO `quietly*`**;
`RJdkForkJoin` uses none. So no corpus workload reaches this today. But all five
public members are `public final` on `ForkJoinTask`, so any user code or
third-party library that calls one hangs the VM with no diagnostic.
`grep -rn 'quietly' --include=*.java` over the corpus is the check to re-run
before assuming the latency still holds.

## 2. The surface, from the image

`javap -p java.util.concurrent.ForkJoinTask | grep -i quietly`:

| # | method | descriptor | access |
|---|--------|------------|--------|
| 1 | `quietlyComplete` | `()V` | `public final` |
| 2 | `quietlyJoin` | `()V` | `public final` |
| 3 | `quietlyInvoke` | `()V` | `public final` |
| 4 | `quietlyJoin` | `(JLjava/util/concurrent/TimeUnit;)Z` | `public final`, `throws InterruptedException` |
| 5 | `quietlyJoinUninterruptibly` | `(JLjava/util/concurrent/TimeUnit;)Z` | `public final` |
| 6 | `quietlyJoinPoolInvokeAllTask` | `(J)V` | package-private `final`, `throws InterruptedException` |

W3-4 §4 named the first five. (6) is the sixth, found by this lane's javap
sweep: it is package-private, and its only caller is `ForkJoinPool`'s own
`invokeAll(Collection)` bytecode, which is itself already a Bridge here — so it
is unreachable today. It is registered anyway, because "unreachable via the
current bridge set" is exactly the assumption that made `awaitQuiescence`
silently dead, and it costs one entry per list.

`javap -p java.util.concurrent.CountedCompleter` adds one more:

| method | descriptor | access | registered? |
|--------|------------|--------|-------------|
| `quietlyCompleteRoot` | `()V` | `public final` on `CountedCompleter` | **no — deliberately** |

Its whole body is a `getfield completer` walk to the root followed by
`invokevirtual quietlyComplete:()V`. No `doExec`, no `awaitDone`. Once
`quietlyComplete()` is a native, running that bytecode is *correct* — and it is
the only member whose real body reads the `completer` chain, which this VM does
not model, so a native would have to fabricate the walk it currently gets for
free. Registering it would also require adding `java/util/concurrent/CountedCompleter`
to the **class** sets of both allow-lists, which changes what gets dropped for
every other method on that class under the real-ForkJoinPool opt-in — an
unmeasured widening with no evidence behind it. Leaving it as bytecode is the
narrower change, not the lazier one.

## 3. Why one registration on `ForkJoinTask` covers every receiver

All six are declared on `ForkJoinTask` and the public ones are `final`, so
resolution of an `invokevirtual` from a `RecursiveTask` / `CountedCompleter` /
user-subclass receiver lands on `ForkJoinTask`. Both dispatch gates key on the
**resolved declaring class**, not the constant-pool symbolic class:
`vm/src/runtime/interpreter/dispatch_virtual.rs:3376` passes `declaring_name`
into `force_native_over_real_jdk_bytecode`, and the vtable fast path at `:742`
uses `cached.class_name` from the resolved `CachedBytecodeMethod`.

This is load-bearing rather than incidental for exactly one call: inside
`CountedCompleter.quietlyCompleteRoot()`, javac emits the self-class form
`Method quietlyComplete:()V` — the constant-pool ref names **`CountedCompleter`**,
not `ForkJoinTask`. It reaches the native only because the gates resolve first.
Same argument W3-4 made for `isCompletedAbnormally`.

## 4. The edits

`register_forkjointask_quietly_bridge` rides the L12 hook shape: **one**
registration function called from both boot paths, rather than W3-4's duplicated
pair. So the entry-for-entry grep is **3 hits per triple** here (1 registration +
2 allow-lists), not W3-4's 4.

| # | file | edit |
|---|------|------|
| 1 | `native-builtins/src/phases_early.rs` | new `fjp_state_set_done_preserving_thrown(o)` beside `fjp_state_set_done` |
| 2 | `native-builtins/src/phases_early.rs` | new `pub(crate) fjp_quietly_body(ctx, this)` beside `fjp_join_void_body` |
| 3 | `native-builtins/src/phases_late/concurrent.rs` | new `register_forkjointask_quietly_bridge`, six `register_with_kind(..., Bridge)` rows |
| 4 | `native-builtins/src/phases_late/concurrent.rs` | call it from `register_new15_loom` (synthetic) **and** `register_t19_k3_forkjoinpool_common` (real-JDK essentials) |
| 5 | `native-api/src/registry.rs` | six rows in `keep_real_forkjointask_bridge` |
| 6 | `vm/src/runtime/interpreter/native_override.rs` | the same six rows in `is_forkjoin_native_override` |

(5) and (6) are not optional and not cosmetic. `registry.rs` records the
`awaitQuiescence` precedent: a method present in only one list is **silently
inert**, because the registration is dropped at registration time and the
interpreter's "force the native" then has no native to force.

### The semantics

**`quietlyJoin()V` / `quietlyInvoke()V` / `quietlyJoinPoolInvokeAllTask(J)V`** —
`fjp_quietly_body`: return immediately if the side table says done, otherwise
`fjp_compute_and_complete` and **swallow** the outcome.

"Swallow" means *do not rethrow*, **not** *do not record*. The throwable is
still written to the side table by `fjp_compute_and_complete` →
`fjp_state_set_thrown` before `fjp_quietly_body` ever sees it, so
`getException()` and `isCompletedAbnormally()` stay truthful — which is the
entire point of the family, since a caller of `quietlyJoin()` is *expected* to
interrogate those afterwards. Discarding the throwable would be the same
silent-success bug `fjp_complete_from_outcome` exists to prevent.
`Err(MethodCallFailed::InternalError)` is a VM failure rather than a task
outcome and still propagates.

The done-check reads `fjp_state_flags`, **not** `fjp_state_get_checked`. The
checked variant raises the `CancellationException` proxy for a cancelled task,
and that proxy is a `MethodCallFailed::InternalError` (via
`RuntimeError::IllegalStateException`), not an `ExceptionThrown` — it would sail
straight through the swallow and abort the caller. The real `quietlyJoin()` on a
cancelled task returns silently: its `status` is already negative, so it never
reaches `awaitDone`.

`quietlyInvoke()` shares the body with `quietlyJoin()` for the same reason
`invoke()` and `join()` already share `fjp_join_body`: with completion memoised
in the side table, "run it if it has not run" and "wait until it has run" are
one operation. The done-check is what stops `fork(); quietlyInvoke()` from
executing the body twice.

**`quietlyJoin(J,TimeUnit)Z` / `quietlyJoinUninterruptibly(J,TimeUnit)Z`** —
same body, then `Value::Int(1)`. Both real bodies end `return status < 0`, i.e.
"is it done"; every task here is computed inline on the calling thread, so by
the time control returns it is done and no deadline can have elapsed. Identical
reasoning to the existing `get(JLjava/util/concurrent/TimeUnit;)` registration,
which likewise ignores its timeout. Neither can raise `InterruptedException`
here, because nothing blocks and `Thread.interrupted()` is never consulted.

**Known divergence, accepted:** real `quietlyJoin(0, unit)` on a not-yet-done
task returns `false` without running it. This returns `true` after running it.
That is inherent to the inline model and is already carried by
`get(J,TimeUnit)`; it is a report of what this VM actually did, not a guess.

## 5. `quietlyComplete()` — the member that is not mechanical

Real `quietlyComplete()` is a bare `setDone()`, which ORs one bit into a
**write-once** status word. Two consequences:

* an already-`ABNORMAL` task **stays** abnormal — `setDone()` cannot clear
  `ABNORMAL`, only add `DONE` (which is already set on such a task);
* the raw-result slot is **not** written. `complete(T)` writes it separately, via
  `setRawResult(v); setDone();`.

The obvious implementation, `fjp_state_set_done(this, Value::Object(None))`,
gets both wrong. Its body:

```rust
e.done = true;
e.result = result;                  // clobbers a stashed raw result with null
e.thrown = Value::Object(None);     // ERASES the abnormal record
```

That `thrown` line is there for a good reason on *its* path — it models "the
task produced this value", and normal/abnormal completion are mutually exclusive
in the real status word too. But routed from `quietlyComplete()` it would erase
exactly the abnormal record W3-4 had just made observable: `t.quietlyComplete()`
on a task that had thrown would flip `isCompletedAbnormally()` from `true` to
`false` and `getException()` from the throwable to `null`. The second effect is
the one W3-4 did not name: it would also null a raw result a `CountedCompleter`
had stashed via `setRawResult` before calling `tryComplete()` — the exact
sequence `java.util.stream.AbstractTask.compute()` uses.

`fjp_state_set_done_preserving_thrown(o)` is the remedy W3-4 §4 specified:

```rust
let e = m.entry(fjp_key(o)).or_insert_with(FjpEntry::new);
e.done = true;
```

Set the done bit, touch nothing else. That is `setDone()`. A cancelled entry is
already `done`, so it is a no-op there — matching the write-once word that
`trySetCancelled` has already stamped. It carries the same bounded 4096/2048
reap as `fjp_state_set_done`, because it is a completion path too and a
`quietlyComplete()`-only workload must not grow the table forever.

Downstream, this is consistent by construction rather than by coincidence:
`fjp_state_get_for_join` consults `fjp_state_thrown` first, so a `join()` after
a `quietlyComplete()` on an abnormal task still rethrows, and on a normal one
still returns the preserved `e.result` rather than the null the naive version
would have left.

### Recorded, not fixed: `complete(Ljava/lang/Object;)V` has the same shape

`ForkJoinTask.complete(T)` is `setRawResult(v); setDone();` — it also leaves
`ABNORMAL` set on an already-abnormal task. All three of this VM's `complete`
registrations (`ForkJoinTask`, `RecursiveTask` in both
`register_forkjoin_natives` and `register_real_jdk_forkjoin_essentials`) call
`fjp_state_set_done`, which clears `thrown`. So `t.complete(v)` on a task that
threw reports a clean completion. That is a real divergence and the fix is
mechanical now that the preserving variant exists — but it is a **behaviour
change to a live, long-standing registration** with a blast radius this lane has
not measured, and it is not the gap this lane was scoped to close. Left as is,
recorded here.

## 6. Verifying the two lists agree

W3-4's shape was `4 hits = 2 registrations + 2 lists`. This lane's is
`3 hits = 1 registration + 2 lists`, because the registration is shared between
boot paths.

```sh
# per name
grep -rn '"quietlyInvoke"'                --include=*.rs .   # 3
grep -rn '"quietlyComplete"'              --include=*.rs .   # 3
grep -rn '"quietlyJoinUninterruptibly"'   --include=*.rs .   # 3
grep -rn '"quietlyJoinPoolInvokeAllTask"' --include=*.rs .   # 3
grep -rn '"quietlyJoin"'                  --include=*.rs .   # 6 — TWO descriptors

# the entry-for-entry check that actually matters
grep -c '"quietly' native-api/src/registry.rs                    # 6
grep -c '"quietly' vm/src/runtime/interpreter/native_override.rs # 6
```

`"quietlyJoin"` is 6 rather than 3 because it has two descriptors (`()V` and
`(JLjava/util/concurrent/TimeUnit;)Z`) — 2 registration-site occurrences plus 2
in each list. The `get` family already has that shape for the same reason.

The two six-row blocks are textually identical modulo indentation, so
`(name, descriptor)` agreement is by construction; the two `grep -c` counts
being equal at 6 is the check that neither block was partially applied.

## 7. Confidence, and the single falsifying observation

Reasoned from the JDK 25 image (`javap -p -c`, quoted above) and from source;
**nothing here was built or executed** — this lane may not run cargo or the VM.

High confidence on §1 and §2: the disassembly is direct evidence, not
inference — `awaitDone` is literally in `quietlyJoin`'s and `quietlyInvoke`'s
bytecode, and `setDone` is literally the whole of `quietlyComplete`'s.

High confidence on §3: `dispatch_virtual.rs:3376` passing `declaring_name` is
the same mechanism `isCompletedNormally` / `isCompletedAbnormally` already rely
on, and both are proven live by `RJdkForkJoin` passing.

Medium confidence on §5's *sufficiency*: the preserving variant is provably the
right primitive, but the side table has no analogue of the real status word's
`DONE`-without-`ABNORMAL`-without-result state, so a caller that distinguishes
"quietlyComplete'd with no result" from "completed normally with null" cannot be
served by this model at all. No corpus caller does.

**Falsifier:** a probe that does
`t.fork(); t.quietlyJoin(); check(t.isDone())` and observes `false`. That would
mean `fjp_quietly_body` never reached `fjp_compute_and_complete` — i.e. the
registration is inert because one of the two allow-list edits did not land, and
real `awaitDone` bytecode is running (in which case the probe hangs rather than
answering, which is the cheaper discriminator: **a hang means inert, a `false`
means the side table is not being written**).

The paired positive check is
`t.quietlyInvoke(); check(t.isCompletedAbnormally() && t.getException() instanceof ISE)`
on a task whose `compute()` throws — that is the one that proves "swallow"
recorded rather than discarded, and it needs no new registration beyond this
lane's.
