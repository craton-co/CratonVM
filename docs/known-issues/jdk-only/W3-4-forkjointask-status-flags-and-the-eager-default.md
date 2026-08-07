# W3-4 — `ForkJoinTask.isCompletedAbnormally()` was unregistered, and what still
# gates the `CRATONVM_FJP_EAGER_FORK` default

Status: **fix landed (unbuilt, unmeasured)** for the assertion; the default flip
is **prepared but NOT applied**. Wave 3, lane W3-4.

Predecessors: `L12-forkjoinpool-no-workers-awaitdone-hang.md`,
`L19-countedcompleter-lazy-fork-starvation.md`,
`W2-8-forkjointask-invoke-returns-computes-null.md`.

---

## 1. The defect

`regression-suite/src/RJdkForkJoin.java:259`

```java
check(t.isCompletedAbnormally(), "isCompletedAbnormally");
```

failed with `java/lang/AssertionError: isCompletedAbnormally` on the fresh build,
`--jdk-only`, `CRATONVM_FJP_EAGER_FORK=1`, immediately after the three checks
before it passed:

```
CK RJdkForkJoin sum=199990000 fill=24995000
CK RJdkForkJoin leaves=64 completions=127
CK RJdkForkJoin parallelSum=5000050000 evens=2500 keys=[m0, m1, m2, m3]
Exception in thread "main" java/lang/AssertionError: isCompletedAbnormally
```

`ForkJoinTask.isCompletedAbnormally()` is `public final` and its body is
`(status & ABNORMAL) != 0` over the real `status` field. This VM completes every
task in the `fjp_state` side table (`native-builtins/src/phases_early.rs`) and
never writes that field, so real bytecode read 0 and answered `false` for a task
that had *just* handed its caller an `ExecutionException` — line 258 passed and
line 259 failed on the same task.

`grep -rn '"isCompletedAbnormally"' --include=*.rs` returned nothing: the method
was registered nowhere and named in neither allow-list.

### Why it is not `!isCompletedNormally()`

In the real status word `ABNORMAL` is only ever set together with `DONE`
(`trySetCancelled` → `DONE|ABNORMAL|CANCELLED`, `trySetThrown` →
`DONE|ABNORMAL|THROWN`). A task that has not run yet is **neither** normally nor
abnormally completed, so the predicate is

```
done && (cancelled || threw)
```

— the complement of `isCompletedNormally` *over a done task*, not its negation.
`RJdkForkJoin.java:294` (`t3.cancel(false)` on a never-submitted task, then
`t3.isCancelled() && t3.isCompletedAbnormally()`) is the check that distinguishes
the two: `fjp_state_cancel` sets `done=true, cancelled=true` on a fresh entry,
which the correct predicate reports and the naive negation also happens to
report — but the naive negation would additionally claim a *pristine* task is
abnormally completed, which HotSpot does not.

## 2. The fix — three coordinated edits, or it is silently dead

`isCompletedAbnormally` is a `ForkJoinTask` method, so it sits behind two gates
that must agree entry-for-entry. `native-api/src/registry.rs` records the
`awaitQuiescence` precedent: a method present in only one list is inert, because
the registration is dropped and the interpreter's "force the native" then has no
native to force.

| # | file | edit |
|---|------|------|
| 1 | `native-builtins/src/phases_early.rs` | `r.register(fjt, "isCompletedAbnormally", "()Z", …)` in `register_forkjoin_natives` (synthetic path), next to `isCompletedNormally` |
| 2 | `native-builtins/src/phases_early.rs` | the same registration in `register_real_jdk_forkjoin_essentials` (real-JDK / `--jdk-only` path) |
| 3 | `native-api/src/registry.rs` | `\| ("isCompletedAbnormally", "()Z")` in `keep_real_forkjointask_bridge` |
| 4 | `vm/src/runtime/interpreter/native_override.rs` | `\| ("isCompletedAbnormally", "()Z")` in `is_forkjoin_native_override` |

Registered on `java/util/concurrent/ForkJoinTask` **only**, exactly like the
sibling `isCompletedNormally` / `isCancelled`: all three are `public final` in
the real `ForkJoinTask`, so an `invokevirtual` on a `RecursiveTask` /
`RecursiveAction` / user receiver still resolves its declaring class to
`ForkJoinTask`. (`getException` is registered for all three class names, which is
belt-and-braces for the same reason — both shapes work.) The two allow-lists key
on `class_name ∈ {ForkJoinTask, RecursiveTask, RecursiveAction}` and then on
`(method_name, descriptor)`, so one entry each covers every receiver.

Verification that the lists agree:

```sh
grep -rn '"isCompletedNormally"'   --include=*.rs .   # 4 hits: 2 reg + 2 lists
grep -rn '"isCompletedAbnormally"' --include=*.rs .   # must be the same 4 shape
```

### Nothing else in `workerException()` is wrong

Line-by-line against the source, with the sibling checks that already pass as
evidence:

| line | needs | route | verdict |
|------|-------|-------|---------|
| 250 `t.get(T, SECONDS)` | `ExecutionException(cause)` | `fjp_future_get_body` → `fjp_state_get_for_future` → `fjp_execution_exception` | ok (same route as `:114`, which passes) |
| 258 cause type + message | the recorded throwable, unwrapped once | `fjp_state_set_thrown` records the original `IllegalStateException` | ok |
| 259 `isCompletedAbnormally` | — | **unregistered** | THE DEFECT |
| 260 `!isCompletedNormally` | false | `done && !cancelled && !threw`, `threw=true` | ok |
| 261 `getException instanceof ISE` | the original | `fjp_state_thrown` | ok |
| 274 `t2.join()` rethrows unwrapped | raw throwable, no wrapper | `fjp_join_body` → `fjp_state_get_for_join` returns `Err(ExceptionThrown(thrown))` | ok |
| 281 pool still usable | 10 | ordinary `SumTask` | ok |
| 293 `t3.cancel(false)` | true | `fjp_state_cancel` on an unseen entry | ok |
| 294 `isCancelled && isCompletedAbnormally` | true | `done && cancelled` | ok once the registration exists |
| 298 `awaitTermination` | true after shutdown | already exercised at `:132`, which passes | ok |

The anonymous `RecursiveTask<Long>` subclasses at 240/264/285 declare
`protected Long compute()`; javac emits the `()Ljava/lang/Object;` bridge, so
`fjt_entry_point`'s `method_exists(cls, "compute", "()Ljava/lang/Object;")`
matches and the task takes the `ComputeObject` arm — the same arm `SumTask`
already proves works.

## 3. `CRATONVM_FJP_EAGER_FORK` as the default — prepared, not applied

The gate's own three conditions (L19 §"What would justify making the new
behaviour the default"):

### (1) `=1` reaches `PASS` in both modes — still open

This lane's fix removes the assertion at `:259`. `:260`–`:298` are argued
correct above but **unrun**. Condition (1) closes only on an observed
`PASS RJdkForkJoin (26 checks)`.

### (2) Regression guard unchanged A vs B — source verdict

The narrowing still holds. `fjt_fork_counted_completer_eager`
(`native-builtins/src/phases_late/concurrent.rs`) calls
`fjt_is_counted_completer(ctx, this)` and, when false, executes
`fjp_state_mark_queued(this); return Ok(Some(Value::Object(Some(this))))` —
byte-identical to the lazy Bridge. So under `=1`, a receiver that does not
transitively extend `java/util/concurrent/CountedCompleter` cannot observe the
gate at all.

| guard | reaches `fork()`? | `CountedCompleter` receiver? | can `=1` change it? |
|-------|-------------------|------------------------------|---------------------|
| `vm/tests/fjp_recursive.rs` | — | — | **no — the test does not run** |
| `vm/tests/rfjp1_recursive.rs` | — | — | **no — the test does not run** |
| `probes/FjpMatrixProbe.java` | no `.fork()` anywhere; only `invoke`/`submit`/`execute`/`lazySubmit`/`invokeAll(Collection)` | tasks are `RecursiveTask` | no, twice over |
| `regression-suite/src/RJdkExecutors.java` | no `ForkJoin*`, no `Recursive*`, no `parallel` at all | — | no |
| Spring / H2 slice | yes — every parallel stream | `java.util.stream.AbstractTask extends CountedCompleter` | **YES — this is the entire blast radius** |

**The two Rust tests are vacuous today.** `apps/fjp_probe/FjpProbe.java` does not
exist in this tree (`apps/` holds only the suite runners plus `cglib_probe`,
`executor_probe`, `jmx_probe`). `fjp_recursive.rs::ensure_probe_compiled` hits
`if !src.exists() { return false; }` and the test returns early;
`rfjp1_recursive.rs::fjp_probe_classes` returns `None` and the test returns
early. Both print a "skipping" line and pass. They therefore cannot move between
A and B — but note that this also means the recorded justification for lazy fork
("eager fork was overflowing the host stack on deeply-recursive `RecursiveTask`
probes") **has no live guard in this repo**. That is an independent gap, not a
blocker for `=1` (a `RecursiveTask` takes the lazy branch under `=1` regardless).

So condition (2) reduces to exactly one question: **does the Spring/H2 slice
move?** Everything reached by a parallel stream goes eager under `=1`.

### (3) `=all` buys nothing `=1` does not — open, one command

### Closing (2) and (3)

Interleave A-B-B-A on the shared host (a straight A,A,B,B measures the host).

```sh
# (1) — the assertion this lane fixed, both modes
cd regression-suite && javac -d out src/RJdkForkJoin.java
timeout 300 cratonvm --jdk-only -cp out RJdkForkJoin;                            echo "A rc=$?"
CRATONVM_FJP_EAGER_FORK=1 timeout 300 cratonvm --jdk-only -cp out RJdkForkJoin;  echo "B rc=$?"
CRATONVM_FJP_EAGER_FORK=1 timeout 300 cratonvm --real-jdk -cp out RJdkForkJoin;  echo "B rc=$?"
timeout 300 cratonvm --real-jdk -cp out RJdkForkJoin;                            echo "A rc=$?"
# want: PASS RJdkForkJoin (26 checks) on both B arms.

# (2) — the only guard that can move. The two Rust tests are skips; run them
# anyway to confirm they still SKIP rather than silently start failing.
cargo test -p cratonvm-vm --test fjp_recursive --test rfjp1_recursive -- --nocapture
javac -d out ../probes/FjpMatrixProbe.java && \
  timeout 300 cratonvm --jdk-only -cp out FjpMatrixProbe                  > /tmp/mx.A
CRATONVM_FJP_EAGER_FORK=1 timeout 300 cratonvm --jdk-only -cp out FjpMatrixProbe > /tmp/mx.B
diff /tmp/mx.A /tmp/mx.B          # must be empty
javac -d out src/RJdkExecutors.java
timeout 300 cratonvm --jdk-only -cp out RJdkExecutors                     > /tmp/ex.A
CRATONVM_FJP_EAGER_FORK=1 timeout 300 cratonvm --jdk-only -cp out RJdkExecutors  > /tmp/ex.B
diff /tmp/ex.A /tmp/ex.B          # must be empty
# THE ONE THAT MATTERS — whatever Spring/H2 slice is already running, A vs B.
# Every parallel stream in it goes eager under =1.

# (3) — =all must add nothing
CRATONVM_FJP_EAGER_FORK=all timeout 300 cratonvm --jdk-only -cp out RJdkForkJoin; echo "C rc=$?"
CRATONVM_FJP_EAGER_FORK=all timeout 300 cratonvm --real-jdk -cp out RJdkForkJoin; echo "C rc=$?"
```

`CRATONVM_FJP_EAGER_FORK` is a DECLARED flag (`types/src/flag_groups.rs:1063`,
`CRATONVM_THREADS=fjp-eager-fork`) served from the latching process-wide
`VmFlags` snapshot — **set it in the environment before launching the process**;
a `std::env::set_var` after the snapshot is invisible.

### The flip, when all three close — ONE line

`native-builtins/src/phases_late/concurrent.rs`, in `fjt_fork_mode()`, the
`else` of the `let Some(raw) = … else` (the "env var unset" arm):

```rust
        return FjtForkMode::Lazy;          // <- becomes
        return FjtForkMode::CountedCompleterEager;
```

Nothing else moves. `"0"` falls through the `match` to `_ => FjtForkMode::Lazy`,
so `CRATONVM_FJP_EAGER_FORK=0` / `CRATONVM_THREADS=-fjp-eager-fork` remains the
escape hatch, and the knob stays bisectable. The doc comment above
`FJT_EAGER_FORK_ENV` (the `unset / 0 / anything unrecognised → today's lazy fork
(DEFAULT)` table) and `FjtForkMode::Lazy`'s own `/// … and the default` must be
updated in the same commit or the next reader is misled.

**Not applied in this lane by instruction.** Do not flip it on condition (1)
alone.

## 4. Still open, recorded not closed: `quietly*`

`quietlyInvoke()V`, `quietlyJoin()V`, `quietlyComplete()V`,
`quietlyJoin(JLjava/util/concurrent/TimeUnit;)Z` and
`quietlyJoinUninterruptibly(JLjava/util/concurrent/TimeUnit;)Z` (all
`public final` on `ForkJoinTask`) are registered nowhere and named in neither
allow-list, so they run real bytecode into `doExec()` + `awaitDone()` — with no
worker threads that is a **hang**, not a null.

It is latent, not live: wave 2 measured 17 `invoke()` call sites and ZERO
`quietly*` across `ReduceOps` / `ForEachOps` / `MatchOps` / `FindOps` / `Nodes`,
and `RJdkForkJoin` uses none. `grep -rn 'quietly' --include=*.java` over the
corpus is the check to re-run before assuming that still holds.

Deliberately **not** landed here, for two reasons:

* A partial surface is the recorded `StampedLock` failure mode — the five
  methods have to go in as one unit.
* `quietlyComplete()` is *not* mechanical. Real `setDone()` ORs `DONE` into a
  write-once status word, so on an already-abnormal task it leaves `ABNORMAL`
  set; the obvious implementation `fjp_state_set_done(this, Value::Object(None))`
  **clears** the recorded throwable (see its `e.thrown = Value::Object(None);`).
  Landing that unmeasured would corrupt exactly the abnormal-completion state
  this lane just made observable.

Recipe when a real call site is measured — same three-edit shape as §2, five
entries each in `keep_real_forkjointask_bridge` and `is_forkjoin_native_override`:

* `quietlyInvoke()V` / `quietlyJoin()V` — drive via `fjp_join_body` and
  **discard** the outcome, including `Err(ExceptionThrown(_))`; return
  `Value::Object(None)`. An internal VM error must still propagate.
* `quietlyJoin(J,TimeUnit)Z` / `quietlyJoinUninterruptibly(J,TimeUnit)Z` — same,
  then `Value::Int(1)`: every task is computed inline, so the timeout never
  applies (identical reasoning to the existing `get(J,TimeUnit)` registration).
* `quietlyComplete()V` — needs a new `fjp_state_set_done_preserving_thrown`, or
  an explicit no-op when the entry is already `done`. Do not reuse
  `fjp_state_set_done`.

## 5. Confidence, and the single falsifying observation

Reasoned from source; **nothing here was built or executed** (this lane may not
run cargo or the VM).

High confidence on §2: `isCompletedAbnormally` is `final`, so the declaring class
is unambiguously `ForkJoinTask`; its sibling `isCompletedNormally` is already
proven live end-to-end by `RJdkForkJoin.java:115` passing; and the side table
already carries both bits the predicate needs (`getException` reads the very same
`thrown`).

High confidence on §3's (2): the two Rust tests skip on a missing fixture, and
neither Java guard contains a `.fork()` or a `CountedCompleter`.

**Falsifier:** a B arm that gets past `:259` and fails at `:260`
(`not completed normally`) or `:294` (`cancelled flags`). That would mean
`ForkJoinPool.submit` is not routing through `fjp_compute_for_submit` /
`fjp_state_set_thrown` at all — i.e. the side table has no abnormal record to
read, and the bug is upstream of the registration this lane added, not in it.
The cheap discriminator is `:261` (`getException instanceof IllegalStateException`):
it reads the same `fjp_state_thrown` and needs no new registration, so if `:261`
also fails, the record was never written.
