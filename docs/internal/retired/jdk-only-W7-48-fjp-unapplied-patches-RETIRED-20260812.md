# W7-48 — the ForkJoin cluster's "unapplied" patches were all applied; what was
# missing was the evidence

> # RETIRED 2026-08-12 — moved out of docs/known-issues/jdk-only/
>
> **Evidence:** RETIREMENT-20260812B.md §1.4. The condition this retirement was
> made conditional on is **satisfied and was confirmed, not assumed**: W6-9
> §7.5's `ForkJoinPool.invoke` / `join()` divergence now has its own row in
> README §2.2, under W6-9. And this record's own deliverable is verified by a
> run rather than by its author's reading — `completionRecord()` is in
> `regression-suite/src/RJdkForkJoin.java:370` with all three RED-before
> assertions (`:388`, `:410`, `:436`), the vector is in `JDKONLY_CLASSES`
> (`run.sh:119`), and `RJdkForkJoin PASS` in **both** arms of the closing suite.
> Its remaining §7 items are owned by W3-4, W6-9 and W7-30, all live.

Status: **audit closed; coverage added (unbuilt, unmeasured)**. Wave 7, lane
W7-48.

Handed the two records `W6-9-complete-erases-the-abnormal-record.md` and
`W3-4-forkjointask-status-flags-and-the-eager-default.md` with the instruction
to apply the verbatim patches each recorded as written-but-not-applied.

**Every one of them was already applied.** Nine patches across the two records,
zero still pending. What both records were actually missing is the other half:
not one of the defects they fixed had a test that fails without the fix. That is
what this lane produced.

---

## 1. Census of the verbatim patches

Nine, counted as each record numbers them. `git log -S` on a distinctive string
from each patch names the commit that landed it.

| # | patch | target | landed by | verdict |
|---|---|---|---|---|
| W6-9 §7.1 | `getException()` on a cancelled task | `phases_late/concurrent.rs` | `849034341` | already applied |
| W6-9 §7.2 | `reinitialize()` registration | `phases_late/concurrent.rs` | `849034341` | already applied |
| W6-9 §7.3 | `RecursiveAction.complete(Object)V` | `phases_late/concurrent.rs` | `849034341` | already applied |
| W6-9 §8.1 | `fjp_state_set_raw_result` + the `fjp_complete_body` call | `phases_early.rs` | `e643b5893` | already applied — **and inert, see §3** |
| W6-9 §8.2 | delete the shadowed `getException` registration | `phases_early.rs` | `e643b5893` | already applied |
| W6-9 §8.3 | allow-list `("reinitialize", "()V")` | `native-api/src/registry.rs` | `e643b5893` | already applied |
| W6-9 §8.4 | the other half of 8.3 | `vm/src/runtime/interpreter/native_override.rs` | `e643b5893` | already applied |
| W3-4 §2 | `isCompletedAbnormally` — four coordinated edits | 2 registrars + 2 lists | pre-existing | already applied |
| W3-4 §3 | the `CRATONVM_FJP_EAGER_FORK` default flip | `phases_late/concurrent.rs` | 2026-08-07 | already applied |

**Applied cleanly by this lane: 0. Needed adaptation: 0. Already dead: 9.**

`e643b5893` ("apply the four cross-file patches wave-1 lanes could not reach")
landed W6-9 §8 in full on 2026-08-11 at 18:27, hours after the record itself was
last edited. The record's own status line was never updated, which is how it
came to be handed out as pending work a day later. That is the whole story of
this lane's first half, and it is worth naming as a shape: **a record that
prescribes a patch does not learn that the patch landed.** The status line is
the only thing a reader checks and it is the thing nobody edits.

### The patches were not merely present — they were checked

Being applied is not the same as being correct, and both records are old enough
that the surroundings could have moved. Verified against the tree as it stands:

* **Both allow-lists agree entry for entry** on all eighteen triples, including
  the three this cluster added (`isCompletedAbnormally`, `completeExceptionally`,
  `reinitialize`). This is the `awaitQuiescence` failure mode the records keep
  citing — one list without the other is inert — and it does not currently
  obtain.
* **Registration order still puts the late bridges last on BOTH boot paths.**
  `register_phase51_natives` (which reaches `register_forkjoin_natives`) is
  called at `native-builtins/src/lib.rs:23581` and `register_new15_loom` at
  `:23677`; `register_real_jdk_forkjoin_essentials` at `:9959` and
  `register_t19_k3_forkjoinpool_common` at `:9978`. So
  `register_forkjointask_w6_9_residual_bridge` and
  `register_forkjointask_eager_fork_gate` are the last writers for every triple
  they touch. Registration being last-write-wins, this is the difference between
  the W6-9 §7 fixes being live and being decoration.
* **No competing `ForkJoinTask.fork()` registrar.** Every other `"fork"`
  registration in the tree is `StructuredTaskScope`-family: a different class and
  a different descriptor (`native-builtins/src/jdk25_concurrency.rs`,
  `util_concurrent_ext.rs`, and the `sts`/`sos`/`sof` blocks in
  `phases_late/concurrent.rs`). Nothing shadows the eager-fork gate.
* **`NativeKind` block boundaries are clean.** Both registrars save
  `r.current_category()`, set `Bridge`, and restore on the single exit path; and
  every individual registration inside them uses `register_with_kind(...,
  NativeKind::Bridge)` rather than relying on the ambient value. The category
  matters because `keep_real_forkjointask_bridge` keeps only Bridges, so an
  unstated re-registration would downgrade the slot and drop it under
  `CRATONVM_REAL_FORKJOINPOOL`. Belt and braces, correctly.
* **Nothing in this cluster is retired.** `native-api/src/retired_shadow.rs`
  contains no `java/util/concurrent` triple at all — the only `ForkJoin` string
  in the file is a vector name in a header comment. So the Bridge retags in this
  family made rows *eligible* for retirement and retired nothing, exactly as
  expected: retirement is the explicit table, never the category.

## 2. What was actually missing: the RED

Both records write out falsifiers (W6-9 §6 and §7.4). Neither falsifier is in
the tree. Before this lane, `regression-suite/src/RJdkForkJoin.java` contained no
`completeExceptionally`, no `reinitialize`, no `getException()` on a *cancelled*
task, and **no call to `complete()` at all** — W6-9 §4's blast-radius survey says
so in as many words, and it stayed true after the fix landed. The two Rust
guards `vm/tests/fjp_recursive.rs` and `vm/tests/rfjp1_recursive.rs` are the
vacuous pair W3-4 §3 already documents: their fixture `apps/fjp_probe/
FjpProbe.java` does not exist, so both take a "skipping" branch and pass in
0.00s.

So seven landed fixes had zero coverage, in a cluster whose dominant defect
species is *a fabricated success where the spec mandates a failure* — the exact
species that a missing test cannot catch.

`completionRecord()` in `regression-suite/src/RJdkForkJoin.java` adds three,
chosen because each was RED before the commits above and each discriminates the
defect from the fix. The vector is in `JDKONLY_CLASSES` in
`regression-suite/run.sh`, so the strict suite runs it and diffs its `CK`/`PASS`
lines against HotSpot; check count moves 26 -> 40.

### 2.1 `complete(v)` must not erase the abnormal record — W6-9 §6

```java
t.completeExceptionally(new IllegalStateException("ce-boom"));
check(t.isDone(),                "completeExceptionally must complete the task");
check(t.isCompletedAbnormally(), "completeExceptionally completes ABNORMALLY");
t.complete(99);
check(t.isCompletedAbnormally(), "the abnormal record survives a later complete()");
check(!t.isCompletedNormally(),  "complete() must not fabricate a normal completion");
check(t.getException() instanceof IllegalStateException, ...);
// join() replays the throwable, not the 99.
```

**Failing output before the fix**, from W6-9 §2 and §6: `isCompletedAbnormally()`
answered `false` and `getException()` answered `null`, for two independent
reasons — `fjp_state_set_done` ended with `e.thrown = Value::Object(None)`, and
`completeExceptionally` was registered nowhere and named in neither allow-list
(`grep -c '"completeExceptionally"'` was `0` on both), so real bytecode CASed the
real `aux`/`status` that nothing in this VM reads. So the FIRST assertion to fire
would have been `completeExceptionally must complete the task` — the task stayed
`done == false` and a later `join()` would have run its body and returned 1.

The two assertions **before** `complete(99)` are the design of this test, not
padding. Without them the three after it cannot distinguish "complete() erased
the record" from "completeExceptionally never wrote one" — two different defects
that produce identical output, and both were live.

### 2.2 A cancelled task must SAY what went wrong — W6-9 §7.1

```java
check(c.cancel(false), ...);
check(c.isCancelled() && c.isCompletedAbnormally(), "a cancelled task is abnormal");
Throwable cancelledEx = c.getException();
check(cancelledEx instanceof CancellationException, ...);
```

**Failing output before:** `getException()` answered `null`, so
`cancelledEx instanceof CancellationException` was false. This is the assertion
the vacuous-green trap is about: `RJdkForkJoin.java:294` already asserted
`isCancelled() && isCompletedAbnormally()` and passed throughout, because that
pair reads the flags. The defect was that the third accessor over the same task
disagreed with them. Asserting the flags again would have been green before and
after; asking what the exception IS is the assertion that moves.

The `CK` line prints `cancelledEx.getClass().getName()`, so the cross-VM diff
also catches "some other throwable" rather than only "not null".

### 2.3 `reinitialize()` must re-RUN, not replay — W6-9 §7.2

```java
check(r.invoke() == 7, "first invoke");
check(runs.get() == 1, "compute() ran once");
r.reinitialize();
check(!r.isDone(),     "reinitialize() un-completes the task");
check(r.invoke() == 7, "second invoke");
check(runs.get() == 2, "reinitialize() makes compute() RUN again");
```

**Failing output before:** `reinitialize` was registered nowhere, so real
bytecode cleared the real `status`/`aux` — which this model never reads — and the
`fjp_state` entry survived. `isDone()` stayed `true`, so
`reinitialize() un-completes the task` fired first; had it not, the second
`invoke()` would have handed back the memoised 7 without running `compute()`.

`runs.get() == 2` is the whole test. `r.invoke() == 7` is true under the defect
too, because the replay returns the same number — a test that asserted only the
value would have been green before and after. This is the same trap as 2.2, one
step subtler: the observable is right and the mechanism is wrong.

### 2.4 What was deliberately NOT added

W6-9 §7.3 (`RecursiveAction.complete(Object)V`) is a **synthetic-mode-only**
hole, by its own §7.3: in real-JDK mode `RecursiveAction` does not declare
`complete`, so the resolved declaring class is `ForkJoinTask` and that
registration already covered an `ra` receiver. The strict corpus runs
`--jdk-only`, i.e. real-JDK, so an assertion there would be green before and
after — coverage-shaped and worth nothing. Left uncovered and said so, rather
than added and counted.

W6-9 §8.1 is not coverable at all; see §3.

## 3. §8.1 is applied and cannot be reached from Java

The one finding here that is not bookkeeping.

W6-9 §7.5/§8.1 diagnosed that `t.cancel(false); t.complete(v); t.getRawResult()`
answered null where the real one answers `v`, because `fjp_complete_body`'s
side-table arm called only `fjp_state_set_done`, which no-ops on a cancelled
entry. The cure — a separate `fjp_state_set_raw_result` called first, since only
`setDone` is write-once — is right, and is applied.

**The arm it fixes has no reachable caller.** `fjt_has_own_raw_result_slot`
takes the side-table arm only when the receiver's runtime class name is one of
`ForkJoinTask` / `RecursiveTask` / `RecursiveAction`. All three are **abstract**.
No Java program can produce an instance whose runtime class is one of them. And a
user subclass does not fall through to the arm by accident either:
`NativeContext::method_exists` walks the superclass chain
(`vm/src/vm/vm_exec.rs`), so for a `RecursiveTask` subclass it finds the
inherited `protected final setRawResult` and the predicate answers `true` — the
virtual arm — which resolves straight back to the registered
`RecursiveTask.setRawResult` native and performs the same side-table write.

So the divergence §7.5 described was correctly reasoned and never observable,
and the patch is defence in depth. Two things follow:

* The predicate's header comment was **wrong about why it works**: it claims
  "every OTHER receiver declares its own field", and a `RecursiveTask` /
  `RecursiveAction` subclass declares no such field and cannot — `setRawResult`
  is `final` on both. Corrected in place, with the reachability measurement, so
  the next reader does not re-derive it.
* A corpus assertion for §8.1 would be **green before and after**. Not written.
  The tempting one — `cancel(false); complete(55); getRawResult() == 55` — passes
  under the defect, because the virtual arm always ran.

## 4. `CRATONVM_FJP_EAGER_FORK` — the consumer does read it

Asked because this codebase has shipped several flags that moved a number
nothing read. This one is wired, end to end:

```
types/src/flag_groups.rs:1224   E { group: THREADS, token: "fjp-eager-fork",
                                    on_key: Some("CRATONVM_FJP_EAGER_FORK"), .. }
        v  runtime_var_os, served from the latching process-wide VmFlags snapshot
phases_late/concurrent.rs:7636  fjt_fork_mode()  -> FjtForkMode
        v
             :7761  register_forkjointask_eager_fork_gate(r)
             |        Lazy          -> return, registry untouched
             |        CountedCompleterEager -> fjt_fork_counted_completer_eager
             |        AlwaysEager   -> fjt_fork_always_eager
        v  register_with_kind(task_class, "fork", "()L..ForkJoinTask;", callback, Bridge)
             x3 classes, called from register_new15_loom (:7809)
                       and register_t19_k3_forkjoinpool_common (:7844)
        v
   fjt_fork_run_now -> phases_early::fjp_compute_for_submit  (runs compute() at fork time)
```

The branch is real: `fjt_fork_counted_completer_eager` tests
`fjt_is_counted_completer(ctx, this)` and, when false, does exactly what the lazy
Bridge does (`fjp_state_mark_queued`, return the receiver). And the gate is the
LAST writer for the `fork` triple on both boot paths (§1), which is the part that
would silently make it inert — an earlier lazy registration cannot win, and no
other registrar claims the triple.

The default flip W3-4 §3 records is present:
`fjt_fork_mode`'s env-unset arm returns `FjtForkMode::CountedCompleterEager`, and
`"0"` falls through the `match` to `_ => FjtForkMode::Lazy`, so the opt-out and
the bisection knob both survive. The `FJT_EAGER_FORK_ENV` mode table above the
enum reads the flipped way too — W3-4 records that it did NOT for four days after
the flip and was corrected on 2026-08-11.

Flag surface, the four things a `CRATONVM_*` name needs: `types/src/
flag_groups.rs:1224`, `types/tests/flag-surface.txt:426`, `docs/flag-tokens.md:
816`, `docs/config/flag-inventory.md:1030`. All four present; this lane added no
flag and needed to regenerate nothing.

**One caveat, unchanged and still open.** W3-4 §3's condition (2) reduced to a
single question — does the Spring/H2 slice move? — and it has still not been run.
The knob is read; what it costs on the only workload that can observe it is not
measured. That is a measurement gap, not a wiring gap.

## 5. Compatible mode, and blast radius per change

`--real-jdk` is contractually frozen except for genuine HotSpot-parity fixes.

| change | touches Compatible? | blast radius |
|---|---|---|
| `completionRecord()` in `RJdkForkJoin.java` | **No.** A test vector. It executes in whatever mode the runner picks and changes no VM behaviour in either. | The vector's own `PASS`/`CK` lines. `checks` moves 26 -> 40, and one new `CK` line is added; `regression-suite/run.sh` diffs `PASS`/`CK` against HotSpot rather than against a stored count, so no golden file needs updating. If it comes back red on a CratonVM binary, that is a real divergence in the fixes of `849034341` / `e643b5893`, not a bad vector — all three assertions are ordinary JDK contract and pass on HotSpot 25 by construction. |
| `fjt_has_own_raw_result_slot` header comment | **No.** A doc comment; no code, no registration, no list. | Zero at runtime. `types/tests/doc_citation_paths.rs` scans Rust comments for doc citations — the added text cites `W7-48` prefix-less and as plain text, with no markdown link, as the gate requires. |
| W6-9 / W3-4 status-line updates | **No.** Documentation. | Zero. |

**No registration, allow-list entry, `NativeKind` or native body was changed by
this lane**, which is why the table is short. The seven fixes whose blast radius
W6-7 declined to estimate were landed and measured by W6-9 §4 and §7 and by
`e643b5893`; this lane re-verified that survey holds (§1) and did not extend it.

The one blast-radius statement worth repeating because it is a behaviour change
already shipped and now under test for the first time: **every cancelled task now
answers non-null from `getException()`** (W6-9 §7.1). W6-9 §4 surveyed the tree
and found no Java or Rust source reading `getException()` on a *cancelled* task.
`RJdkForkJoin.java:261` and `probes/FjpMatrixProbe.java:167-182` reach the record
through a throwing `compute()`; `:294` reads the cancelled flags without asking
for the exception. That survey is still accurate — and §2.2 is now the first
caller in the tree that does ask, deliberately.

## 6. Falsifier for this record

A `git log -S` on any of the nine patch strings returning no commit, or
`grep -c` on the counts W6-9 §5 and §8.4 predict returning anything other than
their stated values. Currently:

```
grep -c '"completeExceptionally"' native-api/src/registry.rs                    -> 1
grep -c '"completeExceptionally"' vm/src/runtime/interpreter/native_override.rs -> 1
grep -c '"completeExceptionally"' native-builtins/src/phases_early.rs           -> 4
grep -c '"reinitialize"'          native-api/src/registry.rs                    -> 1
grep -c '"reinitialize"'          vm/src/runtime/interpreter/native_override.rs -> 1
grep -c '"reinitialize"'          native-builtins/src/phases_late/concurrent.rs -> 2
grep -c '"getException"'          native-builtins/src/phases_early.rs           -> 0
```

Each is the number the two records predict, except one. `reinitialize` is 2 in
`phases_late/concurrent.rs` where W6-9 §8.4 predicted 1: the registration inside
the three-class loop, plus one occurrence in the registrar's own ALLOW-LISTS
block comment. That comment is worth naming, because when this lane arrived it
read

> **`("reinitialize", "()V")` is in neither**, and … until the two one-line
> entries recorded in W6-9 §8 land, the `reinitialize` registration below is
> live only under `CRATONVM_SYNTHETIC_FORKJOINPOOL`.

which had been false since `e643b5893` landed both entries. A reader auditing
this cluster would have concluded the registration was inert in real-JDK mode and
gone looking for a fix that was already in the tree — the same trap that produced
this lane. Rewritten, along with two neighbouring claims in the same block that
the same commit had falsified (the `getException` deletion, and §8.1 being
outstanding). A comment outliving its defect, three times in one block comment.

For §2's coverage the falsifier is the ordinary one: revert either `849034341` or
`e643b5893` and `RJdkForkJoin` must fail, at
`completeExceptionally must complete the task`,
`a cancelled task SAYS what went wrong`, or
`reinitialize() un-completes the task` respectively. **This has not been run** —
this lane could not build or execute a VM.

## 7. What is still open in this cluster

* W3-4 §3 condition (2): the Spring/H2 A/B for the eager-fork default. Named in
  the source and in W3-4; still unrun.
* W6-9 §7.5's underlying divergence, explicitly deferred there and untouched
  here: `ForkJoinPool.invoke` reads `fjp_state_get` where `join()` reads
  `fjp_state_get_checked`, so the two disagree about a cancelled task — the real
  `pool.invoke` is `externalSubmit(task); return task.join();` and `join()` on a
  cancelled task throws `CancellationException`. Fixing it changes what every
  cancelled-task `pool.invoke` site sees, from a value to a raised exception, and
  needs its own blast-radius survey.
* W6-9 §7.3's synthetic-mode `RecursiveAction.complete` path has no assertion in
  any suite that runs (§2.4). Closing it needs a `--synthetic-jdk` vector, which
  this corpus does not currently have a slot for.
* W6-9 §8.2's note that `native-builtins/tests/duplicate_registration_gate.rs`
  would fire is **stale**: it names `BASELINE_SHADOWED_NO_MANAGEMENT` at `1148`
  and the constant now reads `1150`, re-seeded by `b6f0bca44`. The gate is still
  not wired into CI, per its own header.
