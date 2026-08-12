# W3-4 — `ForkJoinTask.isCompletedAbnormally()` was unregistered, and what still
# gates the `CRATONVM_FJP_EAGER_FORK` default

<!-- merge: both sides kept; the lane's finding and the reconciliation's commit attribution are complementary -->
Status: **everything this record specifies is applied; nothing here is pending.**
§2's four coordinated edits are in the tree; the default flip LANDED 2026-08-07;
§4 (`quietly*`) was closed by `W6-7-forkjointask-quietly-family.md`. §3 and §4
are kept as the record of what was measured and what W6-7 was handed. Wave 3,
lane W3-4.

> **2026-08-12 — re-audited by W7-48, and the eager-fork flag is genuinely
> read.** `CRATONVM_FJP_EAGER_FORK` was traced flag-to-consumer, because this
> codebase has shipped levers that moved a number nothing read:
> `types/src/flag_groups.rs:1224` -> `runtime_var_os` -> `fjt_fork_mode()` ->
> `register_forkjointask_eager_fork_gate`, which re-registers the `fork()` triple
> on all three task classes with `fjt_fork_counted_completer_eager` (or
> `fjt_fork_always_eager`), and that callback reaches
> `phases_early::fjp_compute_for_submit` — it really runs `compute()` at fork
> time. The gate is also the LAST writer for that triple on both boot paths
> (`register_phase51_natives` :23581 before `register_new15_loom` :23677;
> `register_real_jdk_forkjoin_essentials` :9959 before
> `register_t19_k3_forkjoinpool_common` :9978, both `native-builtins/src/lib.rs`),
> and no other registrar in the tree claims
> `("fork", "()Ljava/util/concurrent/ForkJoinTask;")` — every other `"fork"`
> registration is `StructuredTaskScope`-family, a different class and descriptor.
> All four flag-surface obligations are met and unchanged.
>
> **What is still NOT closed is §3's condition (2): the Spring/H2 A/B has never
> been run.** The lever is wired; its cost on the only workload that can observe
> it is unmeasured. See `W7-48-fjp-unapplied-patches.md` §4.
>
> §2's `isCompletedAbnormally` now also has a neighbour that discriminates it:
> `regression-suite/src/RJdkForkJoin.java::completionRecord()` asserts the
> abnormal RECORD, not just the flags — `:294`'s
> `isCancelled() && isCompletedAbnormally()` was green throughout the W6-9
> defects, which is exactly why it could not catch them.
**Status (reconciled 2026-08-12 — W7-55-record-reconciliation.md):**

* **§2, the `isCompletedAbnormally` assertion: CLOSED.** All four coordinated
  edits landed 2026-08-07 in commit `d2f5ee6c0` —
  `native-builtins/src/phases_early.rs:9553` (synthetic registrar) and `:10054`
  (`register_real_jdk_forkjoin_essentials`); `native-api/src/registry.rs:6058`
  (`keep_real_forkjointask_bridge`); and
  `vm/src/runtime/interpreter/native_override.rs:1705`
  (`is_forkjoin_native_override`). Both allow-list entries carry this record's
  own rationale comment.
* **§3, the eager-fork default: FLIPPED. The tree no longer matches the
  "prepared, not applied" reading.** Commit `256d119b4`, one line at
  `native-builtins/src/phases_late/concurrent.rs:7667`. `fjt_fork_mode()`
  (`:7636`) now returns `FjtForkMode::CountedCompleterEager` from its env-unset
  arm; **lazy is the opt-OUT**, via `CRATONVM_FJP_EAGER_FORK=0` or
  `CRATONVM_THREADS=-fjp-eager-fork`, which fall through the match at
  `:7670-7676` to `_ => FjtForkMode::Lazy`. The flag is declared at
  `types/src/flag_groups.rs:1224`. Both doc comments this record demanded move
  did move (`concurrent.rs:7598`, `:7605-7607`). **§3's closing sentence — "It
  was not applied in this lane, by instruction" — is superseded** by the boxed
  note near the head of §3; read §3 only as the inventory the flipping commit
  worked from.
* **§4, the `quietly*` family: CLOSED** by `W6-7-forkjointask-quietly-family.md` —
  `register_forkjointask_quietly_bridge` in
  `native-builtins/src/phases_late/concurrent.rs`, with all six triples named in
  `keep_real_forkjointask_bridge` and `is_forkjoin_native_override`.
* **Residual: STILL OPEN — the flip's blast radius on the Spring/H2 slice.**
  This is the one genuinely live item in the record. Its unverified state is
  written into the source itself at `concurrent.rs:7659-7662`. Settling it needs
  the A-B-B-A recipe in §3, with the Spring/H2 suite as the B arm.
* **Residual: STILL OPEN — the two Rust guards are VACUOUS.** `apps/fjp_probe/`
  does not exist in the tree, so `vm/tests/fjp_recursive.rs` and
  `vm/tests/rfjp1_recursive.rs` both early-return. Lazy fork's original
  justification therefore has **no live guard at all** — and lazy is now the
  non-default path, which makes a regression there cheaper to introduce and
  harder to notice.
* **Cannot adjudicate without a run:** §3's conditions (1) and (3) are asserted
  in an in-source note (`concurrent.rs:7648-7649`, `:7656-7657`) but are not
  re-measurable from source. `cratonvm --jdk-only -cp out RJdkForkJoin`, and the
  same with `--real-jdk`.

Wave 3, lane W3-4.

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

## 3. `CRATONVM_FJP_EAGER_FORK` as the default — FLIPPED 2026-08-07

> **This section is history.** The flip landed on 2026-08-07 and the tree no
> longer matches the "prepared, not applied" reading below.
> `fjt_fork_mode()` (`native-builtins/src/phases_late/concurrent.rs`) now returns
> `FjtForkMode::CountedCompleterEager` from its env-unset arm, so **lazy fork is
> the OPT-OUT** (`CRATONVM_FJP_EAGER_FORK=0` / `CRATONVM_THREADS=
> -fjp-eager-fork`, which falls through the `match` to
> `_ => FjtForkMode::Lazy`). The one-line change §3's last subsection specified
> is the change that was made, and the two doc comments it said must move in the
> same commit — the `FJT_EAGER_FORK_ENV` mode table and `FjtForkMode::Lazy`'s own
> line — now read that way too.
>
> **What was measured before flipping** (the flipping commit's own note in
> `fjt_fork_mode`, i.e. the three conditions below, answered):
>
> 1. `=1` reaches `PASS RJdkForkJoin (26 checks)` in BOTH `--jdk-only` and
>    `--real-jdk` — condition (1), closed by observation;
> 2. `probes/FjpMatrixProbe.java` and `RJdkExecutors` are byte-identical A vs B
>    (the only diff is the flag banner line), and `vm/tests/
>    {fjp_recursive,rfjp1_recursive}.rs` are unchanged — with the same caveat
>    §3's table already recorded, that those two tests are VACUOUS (their probe
>    source `apps/fjp_probe/FjpProbe.java` does not exist, so they take a
>    "skipping" branch and pass in 0.00s);
> 3. `=all` passes too but buys nothing `=1` does not, so the NARROWER gate is
>    what shipped.
>
> **Still unverified, and named as such in the source:** the Spring/H2 slice —
> the entire remaining blast radius, since parallel streams are its only
> `CountedCompleter` users. Its pre-flip state was already broken (every real
> parallel stream starved under lazy fork, because `java.util.stream
> .AbstractTask extends CountedCompleter`), so the flip is expected to repair
> rather than regress it — but that has not been run. The reason the flip was
> taken anyway is the one the source states: leaving it off shipped a
> known-broken default.

The gate's own three conditions (L19 §"What would justify making the new
behaviour the default"), as this lane found them:

### (1) `=1` reaches `PASS` in both modes — was open here, CLOSED 2026-08-07

This lane's fix removes the assertion at `:259`. `:260`–`:298` are argued
correct above but **unrun**. Condition (1) closes only on an observed
`PASS RJdkForkJoin (26 checks)` — which is what the flipping commit recorded, on
both `--jdk-only` and `--real-jdk`.

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

### (3) `=all` buys nothing `=1` does not — CLOSED 2026-08-07

`=all` passed too and bought nothing `=1` did not, so the narrower gate is what
became the default.

### Closing (2) and (3) — the recipe that was run

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

### The flip — ONE line, APPLIED 2026-08-07

`native-builtins/src/phases_late/concurrent.rs`, in `fjt_fork_mode()`, the
`else` of the `let Some(raw) = … else` (the "env var unset" arm):

```rust
        return FjtForkMode::Lazy;          // <- became
        return FjtForkMode::CountedCompleterEager;
```

Nothing else moved. `"0"` falls through the `match` to `_ => FjtForkMode::Lazy`,
so `CRATONVM_FJP_EAGER_FORK=0` / `CRATONVM_THREADS=-fjp-eager-fork` remains the
escape hatch, and the knob stays bisectable. The doc comment above
`FJT_EAGER_FORK_ENV` (the `unset / 0 / anything unrecognised → today's lazy fork
(DEFAULT)` table) and `FjtForkMode::Lazy`'s own `/// … and the default` were the
two places that had to move with it or mislead the next reader. `FjtForkMode
::Lazy`'s line moved with the flip; **the mode table did not, and read
`unset … lazy fork (DEFAULT)` for four days** until it was corrected alongside
this record (2026-08-11). That is the failure this paragraph was written to
prevent, landing anyway — a doc comment one screen above the code it describes
is not automatically read when the code changes.

**It was not applied in this lane, by instruction** — this section is the
inventory the flipping commit worked from, not a pending action.

## 4. `quietly*` — recorded here, CLOSED by W6-7

> **This section is history.** `W6-7-forkjointask-quietly-family.md` took the
> work specified below and landed it as one unit, in
> `native-builtins/src/phases_late/concurrent.rs`'s
> `register_forkjointask_quietly_bridge`, called from BOTH boot paths:
> `quietlyJoin()V` at `:6932`, `quietlyInvoke()V` at `:6951`, the two timed
> `(JLjava/util/concurrent/TimeUnit;)Z` overloads from the loop at `:6975`,
> `quietlyJoinPoolInvokeAllTask(J)V` at `:6997`, and `quietlyComplete()V` at
> `:7021`. All six are named in `keep_real_forkjointask_bridge`
> (`native-api/src/registry.rs`) and `is_forkjoin_native_override`
> (`vm/src/runtime/interpreter/native_override.rs`), so the three-edit rule this
> section specified was satisfied entry for entry.
>
> The two things this section got right and W6-7 kept: the family went in as ONE
> unit, and `quietlyComplete()` did NOT reuse `fjp_state_set_done`. It got the
> new primitive named below,
> `phases_early::fjp_state_set_done_preserving_thrown`.
>
> The thing this section only half-saw: it called `fjp_state_set_done`'s
> `e.thrown = Value::Object(None)` a hazard *for `quietlyComplete`*. It was a
> live defect for `complete(V)` itself, on four registrations that had been
> calling it all along — `W6-9-complete-erases-the-abnormal-record.md`.

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
