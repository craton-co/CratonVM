# W6-9 — `ForkJoinTask.complete(v)` erased the abnormal record, and
# `completeExceptionally` was never registered at all

<!-- merge: both sides kept; the lane's finding and the reconciliation's commit attribution are complementary -->
Status: **ALL patches applied; falsifiers added 2026-08-12 (unbuilt,
unmeasured)**. Wave 6, lane W6-9. Takes the defect W6-7 found and declined
("live long-standing registrations, blast radius unmeasured").

> **2026-08-12 — §8 IS APPLIED. Read this before acting on the section.**
> Every patch in §8 landed on 2026-08-11 in commit `e643b5893` ("apply the four
> cross-file patches wave-1 lanes could not reach"), hours after this file was
> last edited, and the status line below was never updated — so this record was
> handed out as pending work a day later and re-audited from scratch. §8 is
> kept verbatim as the specification each patch was built from; it is **not a
> to-do list**. Nothing in it needs applying.
>
> What was genuinely missing is the other half: none of these fixes had a test
> that fails without them. Three falsifiers are now in
> `regression-suite/src/RJdkForkJoin.java::completionRecord()` — §6's, and
> §7.4's first and second — where the strict suite actually runs them.
> `W7-48-fjp-unapplied-patches.md` records the census, the RED for each, and one
> finding this record got wrong: **§8.1's cure is applied and cannot be reached
> from Java**, because `fjt_has_own_raw_result_slot`'s side-table arm tests three
> ABSTRACT class names and `method_exists` walks the superclass chain, so every
> Java receiver takes the virtual-`setRawResult` arm. §7.5's divergence was
> correctly reasoned and never observable. Do not write a corpus assertion for
> it; it is green before and after.

**2026-08-11 — §7's four divergences are now addressed** (landed in source,
unbuilt). Three are landed in
`native-builtins/src/phases_late/concurrent.rs`
(`register_forkjointask_w6_9_residual_bridge`): `getException()` on a cancelled
task, `reinitialize()`, and the missing `RecursiveAction.complete(Object)`. The
fourth — the `setRawResult` half of `complete(v)` on a cancelled task — and the
three edits the first three imply outside that file were written out verbatim in
§8; they were applied later the same day (see the box above). §7 records the
disposition of each rather than deferring it.
**Status (reconciled 2026-08-12 — W7-55-record-reconciliation.md):**

> **§8 IS FULLY APPLIED. THE "not applied" IN ITS HEADING IS FALSE AND HAS
> ALREADY COST TWO SEPARATE AGENT RUNS.** All four §8 patches landed in commit
> `e643b5893` *fix(jdk-only): apply the four cross-file patches wave-1 lanes
> could not reach*, **hours after this record was last edited**. Anyone handed
> §8 as pending work is being handed work that is done. Checked twice
> independently, 2026-08-12.

* **Headline: CLOSED in source** (commit `b3aca74c8`). §3.1 `fjp_state_set_done`
  no longer clears `thrown` (`native-builtins/src/phases_early.rs:8226`); §3.2
  the shared `fjp_complete_body` with a virtual `setRawResult` for own-slot
  receivers (`:8896`); §3.3 `completeExceptionally` registered on all three
  classes and both boot paths (`phases_early.rs:9577`, `:9652`, `:9709`,
  `:9947`) with both allow-list entries (`native-api/src/registry.rs:6071`,
  `vm/src/runtime/interpreter/native_override.rs:1716`).
* **Residual: CLOSED — all four of §7.** §7.1 `getException()` on a cancelled
  task returns a fresh `CancellationException` — `849034341`, `fjt_get_exception`
  at `native-builtins/src/phases_late/concurrent.rs:7404`, registered `:7478`.
  §7.2 `reinitialize()` registered — `849034341` at `concurrent.rs:7447`/`:7485`,
  made live in real-JDK mode by the two allow-list entries `e643b5893` added
  (`native-api/src/registry.rs:6078`,
  `vm/src/runtime/interpreter/native_override.rs:1721`). §7.3
  `RecursiveAction.complete(Object)` registered — `849034341`,
  `concurrent.rs:7492` inside `register_forkjointask_w6_9_residual_bridge`
  (`:7458`). **§7.5 — whose heading said "NOT LANDED" — landed** in
  `e643b5893`: `fjp_state_set_raw_result` at `phases_early.rs:8221`, called from
  `fjp_complete_body` at `:8905` before `fjp_state_set_done`.
* **Residual: CLOSED — §8.1 through §8.4, every one.** `fjp_state_set_raw_result`
  + its call (`phases_early.rs:8221`, `:8905`); the shadowed `getException`
  deleted from `register_real_jdk_forkjoin_essentials` (`phases_early.rs:9922` —
  `grep '"getException"'` over that file now returns zero); the
  `("reinitialize","()V")` allow-list entry (`native-api/src/registry.rs:6078`);
  and `is_forkjoin_native_override`'s `reinitialize` arm
  (`vm/src/runtime/interpreter/native_override.rs:1721`).
* **Cannot adjudicate without a run — two, and one carries an explicit
  instruction.** (1) §8.2 warned the `duplicate_registration_gate`'s
  `shadowed <= BASELINE_SHADOWED` assertion would fire until 8.2 landed; 8.2 has
  landed, but the gate's current numeric state is a build question:
  `cargo test -p cratonvm-native-builtins --test duplicate_registration_gate`.
  **This record explicitly says not to compute or re-seed that number without a
  real run — honour that.** (2) The §6 and §7.4 falsifiers: build, then
  `cratonvm --jdk-only -cp regression-suite/classes RJdkForkJoin` and
  `--real-jdk`. Note the §6/§7.4 Java snippets are in **no** vector —
  `regression-suite/src/RJdkForkJoin.java` mentions neither `reinitialize` nor
  `CancellationException` — so a green `RJdkForkJoin` does not exercise them.

Wave 6, lane W6-9. Takes the defect W6-7 found and declined ("live long-standing
registrations, blast radius unmeasured").

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
disassembly in §1: it clears `DONE`/`ABNORMAL`/`THROWN` and nulls `aux`). When
this was written **`reinitialize` was registered nowhere** — `grep -rn
'"reinitialize"' --include=*.rs .` was empty — so it was not a caller of this
primitive and could not become one without its own lane. That is what made
deleting the erasure safe rather than merely convenient: there was no reuse path
in the tree that needed it.

**Still true after §7's `reinitialize` landed.** `fjt_reinitialize`
(`native-builtins/src/phases_late/concurrent.rs`) does not call
`fjp_state_set_done` or any other reset variant of it — it REMOVES the entry, so
the reset is "no entry", which every reader already treats as a pristine task.
The erasing variant has gained no caller.

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

**This falsifier is now IN THE TREE**, as part 1 of
`regression-suite/src/RJdkForkJoin.java::completionRecord()`, with the two
`isDone()`/`isCompletedAbnormally()` assertions moved BEFORE the `complete()`
call so the test can tell the two independent reasons apart.

Before: `isCompletedAbnormally()` false and `getException()` null — for two
independent reasons (the erasure, and `completeExceptionally` never reaching the
side table). After: true / false / ISE. `t.join()` must still throw the ISE.

## 7. The four divergences this lane left open — disposition (2026-08-11)

All four were left because each needed its own measurement, not because any was
unclear. Three are now landed in source (unbuilt); the fourth is written out in
§8 and not applied. The disassembly quoted below is `javap -p -c
java.util.concurrent.ForkJoinTask` / `javap -p
java.util.concurrent.RecursiveAction` re-taken against JDK 25.0.3.9 — the same
image §1 used.

### 7.1 `getException()` on a cancelled task answered `null` — LANDED

The one that mattered: `isCompletedAbnormally()` said `true` for a cancelled task
while `getException()` said "nothing went wrong". Two accessors over one task,
opposite verdicts, and the campaign's dominant species (§2) in miniature.

The real `getException(boolean)`, which the `public final getException()`
delegates to with `false`:

```text
   1: getfield status; 6: ifge 16           //  status >= 0          -> null
  10: ldc 65536;  iand; 13: ifne 18         // (status&ABNORMAL)==0  -> null
  19: ldc 131072; iand; 22: ifeq 45         // (status&THROWN)==0    -> 45
  32: aux ifnull 45;    42: aux.ex ifnonnull 53
  45: new java/util/concurrent/CancellationException; <init>()V; areturn
```

Branch 45 is what every cancelled task reaches — `trySetCancelled` ORs
`DONE|ABNORMAL` and never touches `aux` — so the spec'd answer is a **fresh
`CancellationException`**, not null, and not the `IllegalStateException` proxy
either: the proxy exists because `RuntimeError` has no `CancellationException`
variant to *raise*, but nothing stops this VM from *allocating* the real class.
It is constructible in both modes (`<init>()V` is registered for it in
`native-builtins/src/lang_misc.rs` and `native-builtins/src/lib.rs`, and the
synthetic path fabricates the class on demand).

`fjt_get_exception` (`native-builtins/src/phases_late/concurrent.rs`) answers the
recorded throwable first, then `done && cancelled` -> a fresh
`CancellationException`, then null — the real method's own branch order, and now
the same predicate `isCompletedAbnormally()` answers.

It is **not** written back into the side table. `fjp_state_set_thrown` refuses to
record on a cancelled entry anyway, so a write would be a no-op today; if it were
not, `fjp_state_get_for_join` replays a recorded throwable, and an accessor that
silently changed what the next `join()` raises is a worse bug than the one being
fixed.

The behaviour change is exactly the one this bullet flagged — every cancelled
task now answers non-null from `getException()`. That is the point. §4's blast
radius survey applies unchanged: no Java or Rust source in the tree reads
`getException()` on a *cancelled* task (`RJdkForkJoin.java:261` and
`probes/FjpMatrixProbe.java` reach the record through a throwing `compute()`, and
`:294` reads the cancelled flags without asking for the exception).

**Cost:** it re-registers a triple `phases_early.rs` already registers, so the
tree now holds one dead `getException` body. §8's first patch deletes it; see
there for the `duplicate_registration_gate.rs` consequence.

### 7.2 `reinitialize()` was unregistered — LANDED (synthetic mode only until §8)

```text
public void reinitialize();
   0: aconst_null; putfield aux
   5: getfield status; ldc int 16777216; iand; putfield status
```

The only method on the class that clears a status bit, and the bit it keeps is
`1<<24`, the pool-submit marker. Real bytecode cleared the real `status`/`aux`,
which this model never reads, so the `fjp_state` entry survived and the next
`join()` replayed the stale result.

`fjt_reinitialize` REMOVES the entry rather than resetting it in place. "Never
seen" is the state `reinitialize` restores here — every reader already treats a
missing key and a fresh entry identically — and it is the only choice that keeps
`fjp_queued_task_count()` honest, since that count is over `!done` entries and a
reset-in-place entry would report a task in nobody's queue as forked-and-pending.
The `1<<24` bit has no reader in this model. Dropping the key drops a GC root,
which is safe here for a reason that does not generalise: the caller is executing
a method *on* the task, so the task is live on its stack.

**Incomplete without §8.** `("reinitialize", "()V")` is in neither allow-list,
and on the default real-ForkJoinPool path `native-api/src/registry.rs` DROPS any
Bridge on these classes whose triple `keep_real_forkjointask_bridge` does not
name. So until §8's two one-line entries land, this registration is live only
under `CRATONVM_SYNTHETIC_FORKJOINPOOL`. Stated rather than assumed away — a
registration named in neither list is the `awaitQuiescence` failure mode this
file's §5 already records.

### 7.3 `RecursiveAction.complete(Object)` was unregistered — LANDED

```text
public final java.lang.Void getRawResult();
protected final void setRawResult(java.lang.Void);
```

Both **final**, and `setRawResult`'s body is empty. So no subclass can give a
`RecursiveAction` a raw-result slot, and `complete(v)` =
`setRawResult(v); setDone();` reduces on this receiver to exactly `setDone()` —
which is `fjp_state_set_done_preserving_thrown`, the W6-7 primitive, not
`fjp_complete_body`. Routing it through `fjp_complete_body` would park `v` in the
side table where nothing can read it back, because `RecursiveAction
.getRawResult()` is a constant-null registration on both boot paths.

That is the evidence this bullet said the lane did not have, and it also settles
the cost: no allow-list entry, because `("complete", "(Ljava/lang/Object;)V")` is
already named for all three classes in both lists. Real-JDK mode was never
affected (the resolved declaring class for an `ra` receiver is `ForkJoinTask`);
this closes the synthetic-mode hole.

### 7.4 Falsifier for the three landed items

```java
ForkJoinTask<Integer> c = new RecursiveTask<>() { protected Integer compute() { return 1; } };
check(c.cancel(false),                              "cancel reports true");
check(c.isCompletedAbnormally(),                    "cancelled is abnormal");
check(c.getException() instanceof CancellationException, "7.1 — and it SAYS so");

ForkJoinTask<Integer> r = new RecursiveTask<>() { protected Integer compute() { return 7; } };
check(r.invoke() == 7, "first run");
r.reinitialize();
check(!r.isDone(), "7.2 — reinitialize un-completes");
check(r.invoke() == 7, "7.2 — and the body runs again, not a replay");

RecursiveAction a = new RecursiveAction() { protected void compute() {} };
a.completeExceptionally(new IllegalStateException("boom"));
a.complete(null);
check(a.isCompletedAbnormally(), "7.3 — ra.complete is setDone, not an erase");
```

**Parts 1 and 2 of this falsifier are now IN THE TREE** (the cancelled
`getException()`, and the `reinitialize` pair with a run COUNTER added — the
value alone does not discriminate a replay from a re-execution). The
`RecursiveAction` third part is NOT: it is a synthetic-mode-only hole by §7.3's
own argument, and the strict corpus runs `--jdk-only`, so an assertion there
would be green before and after. See `W7-48-fjp-unapplied-patches.md` §2.4.

Before: line 3 answered null; `reinitialize()` left `isDone()` true and the
second `invoke()` replayed the memoised 7 without running `compute()`; under
`--synthetic-jdk` the `a.complete(null)` call had no native at all. The
`reinitialize` pair only discriminates once §8.3/§8.4 land — without them it is
dropped in real-JDK mode, so a green there proves nothing about that mode.

<!-- merge: both sides kept; the lane's finding and the reconciliation's commit attribution are complementary -->
### 7.5 `complete(v)` on a cancelled task is still a full no-op — LANDED 2026-08-11, AND INERT
### 7.5 `complete(v)` on a cancelled task is still a full no-op — ~~NOT LANDED~~ LANDED

> **Reconciled 2026-08-12.** This landed in commit `e643b5893`:
> `fjp_state_set_raw_result` at `native-builtins/src/phases_early.rs:8221`,
> called from `fjp_complete_body` at `:8905` before `fjp_state_set_done`. The
> heading is kept for findability.

Recorded in §8 as an applied-by-someone-else patch, because the write belongs
inside `fjp_complete_body` (`native-builtins/src/phases_early.rs`), outside this
lane's files. It is small; what follows is the verdict on the consequence that
made the original bullet defer it, since "changing it also changes
`ForkJoinPool.invoke`" is a reason to look, not a reason to stop.

**The consequence is acceptable, and here is why.** `ForkJoinPool.invoke(task)`
in this VM does `let (done, cached) = fjp_state_get(task); if done { return
cached }` (`phases_early.rs`), so for a cancelled task it returns the cached
result — null today, `v` after the patch. The real
`ForkJoinPool.invoke` is `externalSubmit(task); return task.join();`, and
`join()` on a cancelled task reaches `reportException` and **throws
`CancellationException`**. So `pool.invoke` on a cancelled task is *already*
wrong in the fabricated-success direction, and the patch swaps one wrong answer
for another wrong answer of the same shape. It does not create the divergence and
it does not deepen it.

The divergence that is actually worth its own lane is the one underneath: that
`pool.invoke` reads `fjp_state_get` where `join()` reads `fjp_state_get_checked`,
so the two disagree about a cancelled task — the same two-accessors-one-task
shape as 7.1, one level up. Naming it here rather than fixing it: it changes what
every cancelled-task `pool.invoke` call site sees, from a value to a raised
exception, and that needs the blast-radius survey §4 did for `complete`.

> **2026-08-12 — THAT DIVERGENCE NOW HAS A README ROW, and until today it did
> not.** It was named in the middle of a section whose heading says "LANDED",
> inside a record whose §8 heading has already been misread twice, so the one
> genuinely open finding in §7.5 was the least findable text in the file.
> `docs/known-issues/jdk-only/README.md` §2.2 carries it as a `W6-9` row —
> the directory's own convention is that a record with a live residual has a row
> naming the residual, not the headline, and this record had **no row at all**.
>
> The row states, and this is the part that must not be lost on the next merge:
> the `pool.invoke` / `join()` disagreement is **not** the §7.5 patch. The §7.5
> patch (`fjp_state_set_raw_result`) landed in `e643b5893` and is **inert** —
> `fjt_has_own_raw_result_slot`'s side-table arm tests three ABSTRACT class names
> and `method_exists` walks the superclass chain, so no Java receiver reaches it
> (`W7-48-fjp-unapplied-patches.md` §3). Anyone who reads "§7.5 landed" and
> closes the section closes the wrong thing. There is also **no vector**:
> `regression-suite/src/RJdkForkJoin.java` never calls `pool.invoke` on a
> cancelled task, so a green `RJdkForkJoin` says nothing about it.
>
> Nothing else in this record was touched, and in particular the
> `duplicate_registration_gate` number is untouched: §8.2 forbids re-seeding it
> without a real run, and README §2.6 repeats that. It is still not re-seeded.

**2026-08-12 — the narrow scope is NARROWER THAN THIS, and the divergence was
never observable.** The paragraph below is right that it is visible only through
a bare `getRawResult()` on a receiver whose raw-result slot IS the side table.
What it missed is that no such receiver exists: `fjt_has_own_raw_result_slot`
takes that arm only when the runtime class NAME is `ForkJoinTask` /
`RecursiveTask` / `RecursiveAction`, all three of which are ABSTRACT, and a user
subclass does not fall through either because `NativeContext::method_exists`
walks the superclass chain and finds the inherited `final setRawResult`. So the
cure landed in `e643b5893` is defence in depth and has no Java falsifier —
`W7-48-fjp-unapplied-patches.md` §3. The original text follows.

The narrow scope is worth stating: the divergence is only visible through a bare
`getRawResult()` on a `ForkJoinTask`/`RecursiveTask` receiver whose raw-result
slot IS the side table. A receiver with its own slot never had the bug —
`fjp_complete_body` invokes the virtual `setRawResult` unconditionally, exactly
like the real `complete`. `RecursiveAction` cannot have it (7.3).

<!-- merge: both sides kept; the lane's finding and the reconciliation's commit attribution are complementary -->
## 8. Out-of-file patch — ALL FOUR APPLIED 2026-08-11 (`e643b5893`)
## 8. Out-of-file patch — ~~(not applied)~~ FULLY APPLIED

> **Reconciled 2026-08-12. Do not hand this section out as pending work.** All
> four patches below landed in commit `e643b5893` *fix(jdk-only): apply the four
> cross-file patches wave-1 lanes could not reach*, hours after this record was
> last edited — and the heading was never updated, so §8 has since been issued
> twice as work that was already done.
>
> | patch | landed | evidence |
> |---|---|---|
> | 8.1 `fjp_state_set_raw_result` + call from `fjp_complete_body` | `e643b5893` | `native-builtins/src/phases_early.rs:8221` (fn), `:8905` (call) |
> | 8.2 delete the shadowed `getException` in `register_real_jdk_forkjoin_essentials` | `e643b5893` | `native-builtins/src/phases_early.rs:9922`; `grep '"getException"'` over that file returns zero |
> | 8.3 allow-list `("reinitialize","()V")` | `e643b5893` | `native-api/src/registry.rs:6078` |
> | 8.4 `is_forkjoin_native_override` `reinitialize` arm | `e643b5893` | `vm/src/runtime/interpreter/native_override.rs:1721` |
>
> Expected-count checks from the record all hold: `grep -c '"reinitialize"'`
> gives 1 in `registry.rs`, 1 in `native_override.rs`, 1 in `concurrent.rs`
> (`:7485`); `grep -c '"completeExceptionally"'` gives 1, 1, and 4
> (`phases_early.rs:9577`, `:9652`, `:9709`, `:9947`).

Four edits, none inside this lane's files. Ordered by what they unblock.

> **Kept verbatim as the specification, not as pending work.** All four landed
> in `e643b5893`. Verified present on 2026-08-12 by `git log -S` on a
> distinctive string from each, plus the `grep -c` counts §8.4 predicts. See
> `W7-48-fjp-unapplied-patches.md` §1 for the census and §3 for the one
> correction: 8.1 is applied and unreachable from Java.

### 8.1 `native-builtins/src/phases_early.rs` — the cancelled-task `setRawResult` (7.5)

Add beside `fjp_state_set_done`:

```rust
/// The `setRawResult(v)` half of `complete(V)`, for a receiver whose
/// raw-result slot IS the side table.
///
/// Split out from [`fjp_state_set_done`] because the two halves have
/// different write-once rules: `setDone()` ORs into a status word that
/// `trySetCancelled` has already stamped, so it is suppressed on a cancelled
/// task — but `setRawResult` is an ordinary virtual call that the real
/// `complete(V)` runs BEFORE it, cancelled or not. Suppressing both made
/// `t.cancel(false); t.complete(v); t.getRawResult()` answer null where the
/// real one answers `v`.
pub(crate) fn fjp_state_set_raw_result(o: ObjectRef, result: Value) {
    let mut m = fjp_state().lock();
    m.entry(fjp_key(o)).or_insert_with(FjpEntry::new).result = result;
}
```

and in `fjp_complete_body`, replace

```rust
    if !fjt_has_own_raw_result_slot(ctx, this) {
        fjp_state_set_done(this, val);
        return Ok(());
    }
```

with

```rust
    if !fjt_has_own_raw_result_slot(ctx, this) {
        // `setRawResult(v); setDone();` — as two calls, because only the
        // second is write-once. `fjp_state_set_done` re-stores `result` for
        // the ordinary path, which is the same value; the extra lock
        // acquisition is on a path with no caller in the whole tree (§4).
        fjp_state_set_raw_result(this, val);
        fjp_state_set_done(this, val);
        return Ok(());
    }
```

No allow-list change: this moves no triple.

### 8.2 `native-builtins/src/phases_early.rs` — delete the shadowed `getException` (7.1)

In `register_real_jdk_forkjoin_essentials`, the `for task_class in [...]` loop
registers `getException` and `completeExceptionally`.
`register_forkjointask_w6_9_residual_bridge` now re-registers `getException` on
the same three classes and runs later on both boot paths, so the body in that
loop is unreachable. Delete **only** the `getException` `r.register(...)` call
(keep the `completeExceptionally` one, and keep the loop), and leave a pointer:

```rust
        // `getException()` is registered by
        // `phases_late::concurrent::register_forkjointask_w6_9_residual_bridge`,
        // which runs later on both boot paths — the body that used to be here
        // answered null for a cancelled task (W6-9 §7.1). Do not re-add one:
        // registration is last-write-wins, so a copy here would look live and
        // be dead.
```

**APPLIED 2026-08-11.** The paragraph below describes the state before that.
Its numbers are also STALE: `BASELINE_SHADOWED_NO_MANAGEMENT` now reads `1150`,
not `1148`, re-seeded by `b6f0bca44`. The gate is still not wired into CI.

**Until this is applied, `native-builtins/tests/duplicate_registration_gate.rs`
measures more shadowed registrations than its frozen baseline and its
`shadowed <= BASELINE_SHADOWED` assertion FIRES** (`BASELINE_SHADOWED_MANAGEMENT`
seeded at `1201`, `..._NO_MANAGEMENT` at `1148`). At least three rows — one per
task class for `getException` — and possibly more, because
`register_forkjointask_w6_9_residual_bridge` rides both boot hooks and both may
run in the measured configuration, in which case it also shadows *itself* on
`reinitialize` and `RecursiveAction.complete`, exactly as the `quietly*` family
already does inside the seeded baseline. **Do not compute the new number** — that
gate prints the paste line from a real run, and a count nobody has taken is the
one thing it says not to seed from.

Applying 8.2 is what keeps the baseline where it is for `getException`;
re-seeding is the fallback, not the answer. The gate is not currently wired into
CI (its own header records that), which is why this is a note rather than a
blocker — but it is precisely the "one duplicate removed and one added" case a
count cannot distinguish from nothing happening.

### 8.3 `native-api/src/registry.rs` — allow-list `reinitialize` (7.2)

In `keep_real_forkjointask_bridge`, immediately after the
`("completeExceptionally", "(Ljava/lang/Throwable;)V")` entry:

```rust
                    // W6-9 §7.2: the only method on the class that CLEARS a
                    // status bit. Unregistered, real bytecode reset the real
                    // `status`/`aux`, which nothing here reads — the side-table
                    // completion survived and the next `join()` replayed the
                    // stale result. Must stay in step with
                    // `is_forkjoin_native_override`.
                    | ("reinitialize", "()V")
```

### 8.4 `vm/src/runtime/interpreter/native_override.rs` — the other half of 8.3

In `is_forkjoin_native_override`, after the same neighbour:

```rust
            // W6-9 §7.2: registered by
            // `native-builtins/src/phases_late/concurrent.rs::
            // register_forkjointask_w6_9_residual_bridge`. Must stay in step
            // with `keep_real_forkjointask_bridge` in native-api/src/registry.rs.
            | ("reinitialize", "()V")
```

8.3 and 8.4 are one edit in two files, per §5's three-edit rule: either alone
leaves `reinitialize` inert in real-JDK mode. Expected counts afterwards:

```
grep -c '"reinitialize"' native-api/src/registry.rs                          -> 1
grep -c '"reinitialize"' vm/src/runtime/interpreter/native_override.rs       -> 1
grep -c '"reinitialize"' native-builtins/src/phases_late/concurrent.rs       -> 1
```

(1 rather than 3 in the last: the registration loops over the three class names.)
