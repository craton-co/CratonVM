# The VM never gave a terminating thread its Java-side cleanup

> **RECONCILED 2026-08-12 (W7-55-record-reconciliation.md).** Of section 10's
> three out-of-file patches: **A** is **APPLIED** — commit `78a0428ef` declared
> `CRATONVM_THREAD_CONTAINERS` at `types/src/flag_groups.rs:1217` (as a
> `Group::THREADS` toggle `thread-containers`, not in `SCALARS` as sketched) and
> moved the read to `cratonvm_types::flags::runtime_var(...)` at
> `native-builtins/src/shared_secrets_bridge.rs:760`. **B** is **NOT APPLIED,
> deliberately and correctly** — `CRATONVM_THREAD_EXIT` appears nowhere outside
> `docs/known-issues/`. **C is NOT APPLIED and is the live item**: the main
> (primordial) thread still never gets `Thread.exit()` —
> `run_thread_exit_shared` has exactly two call sites,
> `vm/src/vm/vm_exec.rs:4630` and `:13234`, both worker-death paths.
>
> * **Also open:** `TerminatingThreadLocal.threadTerminated()` and
>   `StackableScope.popAll()` are now reachable **for the first time** and have
>   never been exercised.
> * **The container half is no longer interlocked off. Flipped 2026-08-12,
>   unrun:** `VM_REMOVES_THREADS_FROM_CONTAINERS` is `true` in
>   `native-builtins/src/shared_secrets_bridge.rs`, so the pair §8 prescribes is
>   now the default arm rather than an A/B. Read
>   docs/known-issues/jdk-only/W7-23-thread-container-registration.md §10 before
>   the suite run: the wrong-flip signature is a hang with no `FAIL` line, and
>   `CRATONVM_THREAD_CONTAINERS=0` un-flips it in the same binary.
> * **§10C is still the live item, and it is now fully diagnosed rather than
>   deferred** — see §10C, rewritten 2026-08-12 with the call chain. The short
>   version: the main thread's death is expressed in `vm-cli/src/main.rs`, in a
>   DIFFERENT CRATE, and `run_thread_exit_shared` is module-private to
>   `vm/src/vm/vm_exec.rs`, so the third call site is a two-file change and
>   neither file could be touched by the lane that diagnosed it. The exact patch
>   is written out.
> * **New residual found 2026-08-12, filed at §12:** the per-thread
>   uncaught-handler SIDE TABLE is not cleared on a clean death, so
>   `getUncaughtExceptionHandler()` on a normally-terminated thread answers
>   differently per mode. §9's "the VM's own dispatch … consults a side table, so
>   it is unaffected" is true of the *dispatch* and false of the *getter*.

> **RE-VERIFIED AGAINST THE TREE 2026-08-12 (later pass, A9 record triage).**
> Read only — nothing was built or run in that pass. Two of this record's
> claims have changed and one is now wrong as written. Every line number below
> is against the **committed** tree at `768ac2de0`; a concurrent lane holds
> uncommitted edits to `native-builtins/src/lang_system.rs`, but they are all
> below line 3200 and do not move `native_thread_start0`.
>
> * **§13.6's patch IS APPLIED. The closing "What is not claimed" is stale.**
>   It is at `native-builtins/src/lang_system.rs:1048-1058`, in
>   `native_thread_start0`, immediately after the `let this = match args.first()`
>   binding and before the `Round-7 CRIT fix #3` block — byte-for-byte the text
>   §13.6 wrote, `has_real_jdk_thread_layout` / `thread_run_state` /
>   `object_num_fields` + slot-2-`Long` arms and all. It landed in commit
>   `0113f2daf` (2026-08-12), the same commit that carries this record. So §13 is
>   **applied, still unbuilt and still unrun by this lane**, not "not applied".
> * **The guard has a false-positive gate that §13.5 did not name, and it is a
>   checked-in test.** `vm/tests/threadpoolexecutor_prestart_regression.rs`
>   drives `cratonvm.ThreadPoolExecutorPrestartProbe` for 2000 prestarted
>   workers and fails if any fresh worker is refused — the Tomcat endpoint
>   prestart shape §13.5 reasoned about from `javap`, asserted rather than
>   argued. That is the instrument that goes red if the registry ever
>   misresolves a fresh mirror.
> * **The vector now asserts the live half too.** `RJdkExecutors`'s
>   `threadExitCleanup()` carries §13.1's three rows: the terminated restart
>   throws (`:427`), `ran` is unchanged after the refusal (`:442`), and
>   `start()` on a still-RUNNING thread throws (`:463`/`:468`).
> * **§10C is still the live item and is still unapplied.**
>   `run_thread_exit_shared` is still `fn` with no `pub`
>   (`vm/src/vm/vm_exec.rs:4998`) and still has exactly two call sites — `:4630`
>   and **`:13356`** (§10C's `:13234` has moved). `vm-cli/src/main.rs` contains
>   no call to it; `begin_main_thread_blocking_region` is at
>   `vm-cli/src/main.rs:4576`, so §10C's insertion window is still open and its
>   patch still applies as written.
> * **§10B is still deliberately not applied** — `CRATONVM_THREAD_EXIT` appears
>   nowhere in the tree outside this record. **§10A stays applied**
>   (`types/src/flag_groups.rs:1217`, `types/tests/flag-surface.txt:741`), and
>   `VM_REMOVES_THREADS_FROM_CONTAINERS` is still `true`
>   (`native-builtins/src/shared_secrets_bridge.rs:765`), so §8's A/B is still
>   the run that has not happened.
> * **§12.2's residual is still live, and its proposed cheap fix is overstated
>   — see the correction appended to §12.2.**

**Status: landed in `vm/src/vm/vm_exec.rs`, unconditional, in every mode.**
This is the second half of the pair whose first half —
`jla_start_in_container`'s container registration in
`native-builtins/src/shared_secrets_bridge.rs` — is interlocked off behind
`VM_REMOVES_THREADS_FROM_CONTAINERS`. Read
docs/known-issues/jdk-only/W7-23-thread-container-registration.md first: it
carries the JDK contract with quoted source and the measurement that made
landing the halves out of order a hang rather than a wrong answer.

Nothing was rebuilt in this lane either. Every number below was taken with
HotSpot Adoptium 25.0.3.9 and with the **pre-change** binary
`C:/craton/CratonVM/target/release/cratonvm.exe` (built 2026-08-11 19:41), and
every one is reproducible from the probe sources in §9.

## 1. The recorded patch was still needed

Checked against the source before applying, because this campaign has fifteen
records claiming a patch was never applied when it already was. This one had
not been:

* No native is registered on `java/lang/Thread.exit` in any registrar
  (`grep '"exit"'` over `native-builtins/src` and `vm/src` finds only
  `java/lang/System.exit(I)V`, `java/lang/Runtime.exit(I)V` and Surefire's
  `ForkedBooter.exit`).
* The platform-thread spawn closure in `NativeContextImpl::thread_start` went
  straight from the uncaught-handler `if let Err(e) = result { … }` block to the
  JFR thread-end event, with nothing in between.
* The virtual-thread mirror in `resume_virtual_continuation` did the same.
* `TerminatingThreadLocal` and `StackableScope` appear in **zero** Rust source
  lines in the whole tree.

So the gap was real and the patch was needed. §7 records where the recorded
patch's surrounding claims do **not** hold.

## 2. Measured: `exit()` never ran, and two obvious oracles are blind

`Thread.exit()` ends in `clearReferences()`, which nulls three `Thread` fields.
That makes an oracle that needs no `StructuredTaskScope`, no `ThreadFlock` and
no container registration — so it isolates this half of the fix from the other
half, which is exactly what a probe for a blocked pair needs.

The first version of `ThreadExitProbe` read `threadLocals` after the worker
died, found `null` on CratonVM, and would have reported *"exit ran"* on all
three arms. It was wrong. The rewritten probe reads every cell **twice**, once
while the worker is alive and parked on a latch and once after it has died, and
scores `null`-after-death as evidence only when the alive read was non-null.
Two of the three cells then declared themselves blind:

| cell (alive -> dead) | HotSpot 25 | Compatible | `--real-jdk` | `--jdk-only` |
|---|---|---|---|---|
| `threadLocals` | set -> null | null -> null **BLIND** | null -> null **BLIND** | null -> null **BLIND** |
| `inheritableThreadLocals` | set -> null | null -> null **BLIND** | null -> null **BLIND** | null -> null **BLIND** |
| **`uncaughtExceptionHandler`** | **set -> null** | **set -> set** | **set -> set** | **set -> set** |

Both rows repeated for a worker that terminates normally and for one whose
`run()` throws; every cell above holds on both paths. 3/3 byte-identical per
arm.

`uncaughtExceptionHandler` is the sound one: the probe sets it itself before
`start()`, the alive read confirms the field is populated and reflectively
readable, and the dead read shows CratonVM still holding it where HotSpot has
cleared it. That is a direct measurement that `Thread.exit()` did not run —
independent of containers, and it stays valid after the fix as the check that
it now does.

The two blind rows are their own finding. `TlStorageProbe` isolates it: on
CratonVM `ThreadLocal.get()` returns the stored value and `tl.getClass()` is the
real `java.lang.ThreadLocal`, yet `Thread.currentThread().threadLocals` is
`null` immediately after `set`. CratonVM's `ThreadLocal` is shimmed
(`native-builtins/src/phases_early.rs`) and stores elsewhere, so the JDK's own
per-thread map field is never populated. §7 draws the consequence.

## 3. Pre-flight: `exit()`'s body does run on this VM

The hazard worth naming is that running Java teardown for the first time
executes code this VM has never executed. That is testable **without a
rebuild**, the same way W7-23 tested `Thread.start(ThreadContainer)`: `exit()`
is ordinary private bytecode that no native shadows, so invoking it
reflectively runs exactly the body the fix reaches. `ExitBodyProbe` invokes it
**in-thread on the current thread**, which is how the VM invokes it.

```
                                    HotSpot 25   Compatible   --real-jdk   --jdk-only
before.uncaughtExceptionHandler        set          set          set          set
exit.invoke                     returned-normally   (same)       (same)       (same)
after.uncaughtExceptionHandler        null         null         null         null
vm.still.usable                        yes          yes          yes          yes
```

3/3 byte-identical. So the method is not an unexecutable body, and it agrees
with HotSpot cell for cell.

**What this does not cover, and it is the important part.** It ran with
`container == null` and `headStackableScopes == null`, because nothing in
CratonVM writes either field today (W7-23 §3: the only writer of
`Thread.container` is the interlocked-off bridge). So it exercised
`clearReferences()` and four null checks — *not* `container.remove(this)`, which
is the whole reason the pair exists. §8 is the A/B that does.

## 4. What landed

`vm/src/vm/vm_exec.rs` only. One shared helper plus two call sites.

`run_thread_exit_shared(shared, thread, thread_obj)` sits next to
`dispatch_uncaught_exception_shared`, which already exists as a shared helper
for this same pair of death paths for the same reason: two open-coded copies
drift. It resolves `java/lang/Thread` with `get_loaded_class_id` (no load — a
thread that is terminating has already run Java), checks `exit()V` is present
with `find_method_recursive`, and invokes it through
`invoke_special_shared_on_class`.

**Resolution.** `exit` is `private void exit()`, so it is resolved on
`java/lang/Thread` itself, never on the receiver's runtime class — a `Thread`
subclass does not inherit a private method into its dispatch surface, and this
matters concretely here because every "virtual" thread on this VM is really a
`ThreadBuilders$BoundVirtualThread` (W7-23 §1). `invoke_special_shared_on_class`
states exactly that: invokespecial semantics, walk to the declaring class,
dispatch there and only there, no iface/abstract retarget. Its pre-resolved form
also skips `load_class_concurrent`'s loader-blind by-name lookup. HotSpot does
the same thing — `JavaThread::exit` calls
`JavaCalls::call_virtual(…, vmSymbols::java_lang_Thread(), exit_method_name, …)`,
and resolving a private method against `java.lang.Thread` is not a vtable
dispatch.

**Presence check.** `exit()V` is looked up before invoking, so a class library
whose `java/lang/Thread` is a stub without it is a silent no-op instead of a
`NoSuchMethodError` WARN on every single thread death. Every mode the shipped
binary supports has the real method: `--real-jdk` is the default, `--jdk-only`
implies it, and `--synthetic-jdk` needs a Cargo feature that is not in the
default set. `thread.exit.declared=true` on all three arms.

## 5. The insertion point, against what each step has already torn down

Both call sites sit **after the uncaught-exception dispatch** and **before the
JFR thread-end event**, which is the first line of teardown on either path.
Every step that follows removes a precondition `exit()` needs, so the position
is forced rather than chosen. `exit()` runs arbitrary Java: it can allocate, it
can reach a safepoint, and `StackableScope.popAll()` is documented in the JDK
source with "this may block".

| step, in closure order | what it takes away | consequence of running `exit()` after it |
|---|---|---|
| uncaught-handler dispatch | — | must come **first**: `clearReferences()` nulls `uncaughtExceptionHandler`, so an earlier `exit()` deletes the handler before it is consulted |
| JFR / JVMTI `ThreadEnd` / JDWP `ThreadDeath` | — | HotSpot posts JVMTI `ThreadEnd` *after* `Thread.exit()`; keeping our order matches it |
| `jvm_thread.tlab.retire()` | stamps a walkable filler over the TLAB tail | not unsound (`retire()` is idempotent, the next carve refills) but pointless churn on a thread already declared finished |
| `enter_inflated_or_contend(wake_obj)` | the thread now **owns** the monitor on its own `Thread` object | Java that blocks while holding it is the three-way deadlock `Monitor::block_enter`'s doc comment warns about, and `block_enter` never checks safepoints |
| `flush_thread_satb()` | drains this thread's SATB buffer | a reference overwritten afterwards is logged into a buffer whose only strong `Arc` dies with the OS thread; the marker loses a gray source it needed |
| `mark_dead()` | the collector stops scanning this thread's frames and locals | Java run past this allocates objects nothing roots — a use-after-free, not a cleanup |
| `release_monitors_held_by_except()` | sweeps every monitor still held | any monitor `exit()` entered is force-released underneath it |

The `ContinuationYield` early return above the insertion point is deliberately
*not* covered: that is a suspension, not a termination, and the thread reaches
this code on the resumption that finally ends it.

**GC.** Both sites re-read the `Thread` object from the registry rather than
reusing the ref captured before `run()`. The registry copy is remapped after
every GC (`update_thread_objs_after_gc`) and the captured one never is, and
`run()` plus the uncaught dispatch is an arbitrarily long window for a moving
collection — the same stale-ref UAF the `wake_obj` read further down the closure
already documents. `invoke_special_shared_on_class` pins the receiver across its
own load/`<clinit>` window, and from there the callee's frame roots it.

## 6. Both paths, and what an exception from `exit()` does

The call is **outside** the `if let Err(..)` arm on both paths, so normal and
abnormal termination both reach it. The abnormal one is the one that gets
forgotten and the one that matters most: a task that ends by throwing is
precisely when a structured-concurrency owner is parked waiting for the flock
count to fall. The JDK covers the third case itself — `Thread.start(ThreadContainer)`'s
`finally { if (!started) container.remove(this); }` handles failed-to-start, and
that body is the one the bridge half invokes, so nothing is needed here for it.

An error out of `exit()` is **reported at WARN and not propagated**, and both
halves of that are deliberate:

* **Not propagated.** Everything after the call is what marks the thread dead
  and notifies the termination monitor that wakes `Thread.join()`. An early
  return would trade a container leak for a certain hang, which is strictly the
  worse failure and the exact trade this whole pair exists to avoid.
* **Not swallowed.** A silent `let _ =` would hide `ThreadFlock.onExit`'s
  `assert removed` firing under `-ea`, which is the signature of a
  double-remove — the opposite regression W7-23's falsifier tells the next
  person to watch for. The WARN names the tid, the thread name and the error,
  and says that a `ThreadContainer` may still hold the thread.

The native-return slot is drained on the error path, for the reason the
uncaught-handler block a few hundred lines down already documents: a leftover
there is read as a substitute for a thrown exception by the next consumer and
misattributes it.

## 7. Where this record disagrees with W7-23

**W7-23 §B overstates what wiring `exit()` makes live.** It lists
`TerminatingThreadLocal.threadTerminated()` as one of three JDK behaviours that
"become live for the first time". It does not. JDK 25's `exit()` gates that call
on `terminatingThreadLocals() != null`, which reads
`holder.terminatingThreadLocals` — and §2 measured that this VM's shimmed
`ThreadLocal` never populates the JDK's per-thread map fields at all
(`threadLocals` is `null` on a live thread that has just called `set`, while
`get` returns the value). So `TerminatingThreadLocal` stays dead after this
fix, for a **second and independent** reason: not "the VM never calls `exit()`"
but "this VM's `ThreadLocal` does not store where the JDK looks". Whoever picks
that up needs a `ThreadLocal` lane, not this one.

For the same reason `clearReferences()`'s first two writes are no-ops here —
`threadLocals` and `inheritableThreadLocals` are already `null`. Its third,
`uncaughtExceptionHandler = null`, is the one with an observable effect, and it
is what §2 and §3 measure.

The rest of W7-23 holds: `StackableScope.popAll()` and `container.remove(this)`
do become reachable, and `container.remove` is the load-bearing one.

**The recorded patch's `find_class_id` / `invoke_on_class_shared` shape** was
written as a shape, not a verified API, and the record said so. The landed form
uses `get_loaded_class_id` + `invoke_special_shared_on_class`; the three
properties W7-23 required (both paths, cannot skip the monitor notify, resolved
on `java/lang/Thread`) all hold.

## 8. The A/B the next person must run, and the falsifier

Nothing here is rebuilt, so this section is an instruction, not a result. In one
binary built from `dev` with both halves in it, `CRATONVM_THREAD_CONTAINERS`
selects the arm.

**Run the cheap container-free check first — it tests this half alone and needs
no environment variable at all.** `ThreadExitProbe`, any mode:

* `ok.uncaughtExceptionHandler` and `bad.uncaughtExceptionHandler` must both
  flip from `set->set exit-did-NOT-run` to `set->null exit-RAN`.
* If either still reads `set->set`, this half is not running — stop, and do not
  touch `CRATONVM_THREAD_CONTAINERS` until it does. `bad.*` still reading
  `set->set` while `ok.*` flips is the specific defect of a cleanup wired only
  into the happy path.
* `threadLocals` / `inheritableThreadLocals` must still read `BLIND`. If they
  report `exit-RAN`, the `ThreadLocal` shim changed and §7's disagreement needs
  re-measuring.

**Then the pair, on `--real-jdk`, `CRATONVM_THREAD_CONTAINERS=1` vs `=0`:**

* `ExitPairingProbe` (W7-23 §7) — **check this one first**, it is the hang and
  it is cheap. `pair.count.afterTermination` must be `0` at `=1` and stay `1` at
  `=0`; `hang.join.returnedAfterMs` must print a number at `=1` where the
  pre-change binary printed `SECTION-HUNG` 3/3.
* `ContainerRegistrationProbe` (W7-23 §7) at `=1`:
  `flock.threadCount.afterFork=1`, `flock.containsThread.inTask=true`,
  `flock.join.blockedAtLeast300ms=true`, `abnormal.threadCount.afterJoin=0`.
* `probes/StructuredTaskScopeProbe.java` at `=1`:
  `joinWaits.taskFinishedWhenJoinReturned` and `joinBlockedAtLeast200ms` become
  `true`, `joinWaits.subtask.get` becomes `done`, `timeout.join` becomes
  `StructuredTaskScope$TimeoutException`. Three runs: the eight unstable lines
  in W7-23 §2 must stop moving, except `allUntil.*`, which moves on HotSpot too.
* One arm with `-ea`, watching for `ThreadFlock.onExit`'s `assert removed` — a
  thread removed twice, or removed from a container it was never added to.
* A Jetty-backed suite. `SharedThreadContainer` never blocks on a count so it
  should be unaffected, but "should" is not a measurement.

**Falsifier for this half.** If `ExitPairingProbe`'s
`pair.count.afterTermination` still reads `1` at `CRATONVM_THREAD_CONTAINERS=1`
while `ThreadExitProbe` reports `exit-RAN`, then `exit()` is running but
`threadContainer()` is answering `null` — the bridge is still not binding the
container, and the defect is in the first half, not this one. If instead
`ThreadExitProbe` reports `exit-RAN` and a WARN line
`Thread.exit() failed on terminating thread …` appears, `exit()` is being
reached and throwing; the error text names which.

## 9. Observable change, precisely, and what could break

**In every mode — `Compatible`, `--real-jdk`, `--jdk-only`, synthetic.** This is
not a strict-mode policy question: every thread in every mode previously skipped
its Java teardown, and now none does. `Compatible` is affected identically.

With the container half at its default (**off**), every terminating thread now
additionally executes: two null field reads (`headStackableScopes`,
`container`), one null accessor call (`terminatingThreadLocals()`), and
`clearReferences()` — four field writes, two of which write `null` over `null`.
The single observable difference is that `Thread.getUncaughtExceptionHandler()`
on a **terminated** thread now returns the `ThreadGroup` instead of a handler
that was set on the instance, and the `nioBlocker` field is nulled. Both are
convergence to HotSpot, which does the same. No new output on a healthy run.

With `CRATONVM_THREAD_CONTAINERS=1` the same call additionally runs
`container.remove(this)`, which is real JDK bytecode reaching
`ThreadFlock.onExit` -> `threads.remove` + `decrementThreadCount` +
`LockSupport.unpark(owner)`.

**What could plausibly break, named rather than assumed benign:**

1. **`StackableScope.popAll()` can block.** It is guarded by
   `headStackableScopes != null`, which nothing on this VM writes today — but
   the day ScopedValue/structured-concurrency machinery does write it, a
   terminating thread starts blocking inside a path that previously could not
   block at all. The insertion point is chosen so that it blocks as a normal
   live mutator (before the termination monitor is acquired, before
   `mark_dead`), which is the only position where blocking is survivable.
2. **`container.remove(this)` runs real JDK collection code on a dying thread.**
   `ThreadFlock.onExit` does a `Set.remove`, a `getAndAdd` on a `VarHandle`, and
   an `unpark`. Any of those hitting a VM defect now does so during thread
   death, where the failure surfaces as a WARN and a leaked container rather
   than at the call site that caused it.
3. **The WARN is per thread death, not per process.** A defect that makes
   `exit()` throw on every thread produces one line per thread. That is
   deliberate — a once-per-process warn would hide a defect that only affects
   some threads — but a pathological run can be noisy.
4. **Nulling `uncaughtExceptionHandler` at death.** Any CratonVM code or test
   that reads the real field on an already-terminated thread now reads `null`.
   The VM's own dispatch runs strictly before this point and consults a side
   table, so it is unaffected; user code that inspects a dead thread's handler
   changes answer, in HotSpot's direction.
5. **Cost.** One extra Java invoke per thread death, into a method whose body is
   four null-checks in the default configuration. Thread-death-heavy workloads
   (Jetty pools, Tomcat) pay it per thread, not per task.

## 10. Out-of-file patches (not applied)

### A. `CRATONVM_THREAD_CONTAINERS` is declared nowhere, and `types/tests/flag_declaration_guard.rs` will fail on it

Found while deciding whether to give this half its own kill switch. It is a
defect in the **first** half, in a file this lane does not own.

`native-builtins/src/shared_secrets_bridge.rs:751` reads

```rust
    *ENABLED.get_or_init(|| match std::env::var("CRATONVM_THREAD_CONTAINERS") {
```

`"CRATONVM_THREAD_CONTAINERS"` is an exact whole-string `CRATONVM_*` literal in
a scanned crate (`flag_declaration_guard.rs`'s `SKIPPED_DIRS` is only `target`,
`.git`, `apps`, `node_modules`), and the name appears in **none** of
`types/src/flag_groups.rs`, `types/tests/flag-surface.txt`,
`docs/flag-tokens.md`, `docs/config/flag-inventory.md`, or the guard's `ALLOWED`
list. So `cargo test -p cratonvm-types` goes red on the pair as it stands. It is
a `cargo test` assertion and not a compile error, so `cargo build --all-targets`
stays green over it — which is how it got here.

It is also a semantic bug, not only a CI one. `flag_declaration_guard.rs`'s own
table: an **undeclared** name is served by live `getenv` rather than the latched
snapshot and is **invisible to `with_thread_overrides`** — so a test that tries
to select an arm through the supported override hook silently gets whatever the
developer's ambient environment says. The A/B in §8 is exactly that kind of test.

Fix is the four-file edit `flag_groups.rs`'s doc comment spells out. Minimum, as
a scalar:

```rust
// types/src/flag_groups.rs, in SCALARS
pub const SCALARS: &[&str] = &[
    "CRATONVM_JAVA_HOME",
    "CRATONVM_BIN",
    "CRATONVM_MAVEN_REPO_LOCAL",
    "CRATONVM_ENABLE_ASSERTIONS",
    "CRATONVM_DISABLE_JIT",
    "CRATONVM_THREAD_CONTAINERS",
];
```

plus the name in sort order in `types/tests/flag-surface.txt` (byte-compared —
match the file's line endings), plus the generated rows in `docs/flag-tokens.md`
and `docs/config/flag-inventory.md` with their counts bumped
(`tools/flag-census/render-tokens.sh` and `render-inventory.py` write those from
the table and beat hand-editing). And the read site must then move from
`std::env::var` to `cratonvm_types::flags::runtime_var`, or check 4 of
`check-surface.sh` trips on a declared name read raw.

### B. A kill switch for this half — deliberately not added

Symmetry with `CRATONVM_THREAD_CONTAINERS` argues for a
`CRATONVM_THREAD_EXIT=0` escape hatch, so §9's "latent defects surface" risk
could be bisected in one binary without a rebuild. It was **not** added, because
adding it correctly is the same four-file edit as §A across three files this
lane does not own, two of which are generated — and adding it incorrectly
reproduces the exact defect §A documents. The lever also should not default off:
a default-off gate is the shape this campaign has already found eleven times as
"class-library gaps" that were really switched-off code
(docs/known-issues/jdk-only/W7-5-registrars-that-never-shipped.md).

If §9's risk materialises, the switch is: declare `CRATONVM_THREAD_EXIT` by §A's
recipe, then at the top of `run_thread_exit_shared`

```rust
    // Default ON. This is an escape hatch for bisecting a suite that regressed
    // the day Java thread teardown started running, NOT a policy gate — `=0`
    // restores the pre-2026-08-11 behaviour exactly, including the container
    // leak, so it must be paired with CRATONVM_THREAD_CONTAINERS=0.
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ENABLED.get_or_init(|| {
        !matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_THREAD_EXIT").as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    }) {
        return;
    }
```

### C. The main thread does not get `exit()` either — and here is the exact call chain

> **REWRITTEN 2026-08-12 with the call chain, which was the whole ask.** The
> earlier text said "CratonVM's main-thread teardown is in a different file (the
> one that owns the other `clear_tlab_addr` call site)". That pointer is
> **wrong**: the other `clear_tlab_addr` call sites are `vm/src/native/jni.rs`'s
> `detach_foreign_thread` (a JNI foreign thread, not the primordial one), and the
> main thread **never reaches `clear_tlab_addr` at all**.

`Thread.exit()` is wired into the two worker death paths in
`vm/src/vm/vm_exec.rs`. HotSpot's `JavaThread::exit` runs on the primordial
thread too, at VM shutdown. CratonVM's does not, and the reason it cannot be
fixed from either of this record's two files is structural, not a matter of
ownership convention.

**Where the main thread's death is expressed.** `vm-cli/src/main.rs`, in
`fn run()`. `main(String[])` is invoked through `vm.invoke(&class_name, "main",
"([Ljava/lang/String;)V", …)` inside a `catch_unwind`, and immediately after it
`run()` enters `phase::Category::VmShutdown` — everything from there to the end
of `run()` is teardown. What that teardown does **not** contain, verified by
grep: no `mark_dead(ThreadId(0))`, no `release_monitors_held_by` for the main
thread, no termination-monitor notify, no `clear_tlab_addr`, and no
`Thread.exit()`. `Vm::begin_main_thread_blocking_region` (`vm/src/vm/vm_init.rs`)
is the only main-thread teardown step there is, and all it does is
`set_vm_state` + `tlab.retire()` before the non-daemon wait. `Drop for Vm` runs
AOT flush, CDS dump, `run_pending_finalizers`, JVMTI `VMDeath` and
`release_vm_native_state` — no thread teardown either.

**Why it is a two-file change.** `run_thread_exit_shared` is declared
`fn`, with no `pub` and no `pub(crate)`, so it is private to the module
`crate::vm::vm_exec`. `vm/src/vm.rs` has `pub(crate) mod vm_exec;` +
`pub use vm_exec::*;`, so raising it to `pub` makes it reachable from `vm-cli` as
`cratonvm_vm::vm::run_thread_exit_shared`; leaving it private makes the call
impossible from anywhere outside that one file, and the main-thread death is not
in that file. Hence: one visibility change in `vm/src/vm/vm_exec.rs`, one call in
`vm-cli/src/main.rs`. Both are out of the ownership of the lane that has this
record.

**The insertion window, derived the same way §5 derives the worker one.** In
`vm-cli/src/main.rs::run()`, after the `let result = match result { Ok(r) => r,
Err(panic) => { … bail!("main() panicked: {msg}"); } };` block — so a Rust panic
in `main()` is already out of the way and `result` is a `MethodCallResult` — and
**before** `vm.begin_main_thread_blocking_region("vm-main:wait-non-daemon")`,
which is the first thing to retire main's TLAB. Placing it there and not later
covers **both** termination paths: the Java-exception path never reaches the
non-daemon wait at all (`if matches!(result, Ok(_))` guards it) and goes straight
to the renderer and `bail!`, so anything placed at or after that guard misses
abnormal termination — the same trap §6 describes for the worker paths.

**The receiver.** Read the main thread's `java.lang.Thread` mirror from the
**registry**, not from `vm.main_thread.java_thread_obj`: the registry copy is
remapped after every GC (`update_thread_objs_after_gc`) and the `JvmThread`
field copy is not, and `main()` is an arbitrarily long window for a moving
collection. `JvmThread::thread_id` is `pub`, so the tid comes from the thread
itself and no `ThreadId` needs constructing:

```rust
    // vm/src/vm/vm_exec.rs — visibility only
-fn run_thread_exit_shared(shared: &SharedVm, thread: &mut JvmThread, thread_obj: ObjectRef) {
+pub fn run_thread_exit_shared(shared: &SharedVm, thread: &mut JvmThread, thread_obj: ObjectRef) {
```

```rust
    // vm-cli/src/main.rs, immediately after the `let result = match result {…};`
    // block and before `begin_main_thread_blocking_region`.
    //
    // W7-27 §10C: HotSpot's `JavaThread::exit` runs `Thread.exit()` on the
    // primordial thread too. Placed here, not after the non-daemon wait,
    // because the Java-exception path never reaches that wait — the abnormal
    // path is the one that gets forgotten. Errors are already reported at WARN
    // inside the helper and never propagated.
    {
        let main_tid = vm.main_thread.thread_id;
        if let Some(main_thread_obj) = vm.shared.threads.thread_registry.java_thread_obj(main_tid) {
            cratonvm_vm::vm::run_thread_exit_shared(
                &vm.shared,
                &mut vm.main_thread,
                main_thread_obj,
            );
        }
    }
```

`(&vm.shared, &mut vm.main_thread)` is a disjoint-field borrow of
`Vm { pub shared: Arc<SharedVm>, pub main_thread: Box<JvmThread> }` and is
already the house pattern two hundred lines above — `ensure_singleton_oom(&vm.shared,
&mut vm.main_thread)` and `invoke_premains(&vm.shared, &mut vm.main_thread, …)`
in the same function. `java_thread_obj(ThreadId) -> Option<ObjectRef>` is `pub`
on `ThreadRegistry`; `SharedVm::threads` and `ThreadRealm::thread_registry` are
both `pub`.

**Whether to land it at all — the honest position.** It matters much less than
the worker halves: the process is ending, so no observer survives to see a
container that was not emptied, and the only observable `clearReferences()` write
(`uncaughtExceptionHandler`) has no reader left either. Against that, this runs
arbitrary Java on the primordial thread during VM shutdown for the first time
ever, and it runs it on **every** program the VM has ever executed rather than
only on ones that start threads. `ExitBodyProbe` (§3) already invokes `exit()`
in-thread on the main thread on all four arms and the VM stays usable, which is
the cheapest available pre-flight and it passes — but that was with
`container == null`, and the container half is no longer interlocked off, so the
pre-flight no longer covers the interesting branch. **Recommended order: land the
container flip first, measure it, and only then take §10C.** Two first-time-ever
Java-teardown changes in one unmeasured wave is a bisect nobody can do.

**Adjacent, and worth knowing before anyone widens this into "run shutdown
teardown properly":** `Runtime.addShutdownHook` registers into
`native-builtins/src/lang_system.rs`'s `SHUTDOWN_HOOKS` and **nothing ever runs
them** — the file says so in place. `java/lang/Shutdown.runHooks` is registered
nowhere and invoked nowhere, and `System.exit`/`Runtime.exit` go straight to
`std::process::exit`. That is a separate, larger gap than this one; do not let it
ride along on §10C.

## 11. Probe sources

Not checked in under `probes/`: this lane owns one source file and one record.
All three probes need `--add-opens java.base/java.lang=ALL-UNNAMED` on both
arms, and nothing else — no `jdk.internal.*` opens, because none of them names
`StructuredTaskScope` or `ThreadFlock`. That is the point: they test this half
in isolation from the interlocked-off half.

`ThreadExitProbe` — §2's table. Reads `threadLocals`,
`inheritableThreadLocals` and `uncaughtExceptionHandler` off a worker twice,
once while it is parked on a `CountDownLatch` after having populated them and
once after it has died, and prints `alive->dead` plus a verdict of `exit-RAN`,
`exit-did-NOT-run`, or `BLIND(alive-read-was-null)`. Two sections: a worker that
returns, and a worker that throws `IllegalStateException` with a per-thread
handler installed. The load-bearing part is the verdict function, because the
first draft of this probe had no alive read and reported the opposite answer:

```java
    static void verdict(String tag, String alive, String dead) {
        String v;
        if (!"set".equals(alive)) v = "BLIND(alive-read-was-" + alive + ")";
        else if ("null".equals(dead)) v = "exit-RAN";
        else v = "exit-did-NOT-run";
        p(tag, alive + "->" + dead + " " + v);
    }
```

`ExitBodyProbe` — §3's table. Sets the current thread's handler, reads the
field, calls `Thread.class.getDeclaredMethod("exit")` `setAccessible(true)` and
invokes it **on the current thread**, reads the field again, then does a
throwaway `StringBuilder` round-trip to show the VM survived.

`TlStorageProbe` — §7's finding. Prints `threadLocals` before and after a
`ThreadLocal.set`, alongside `ThreadLocal.get()` and `tl.getClass().getName()`,
so a functioning `ThreadLocal` whose storage is not the JDK's field is
distinguishable from a broken one.

## 12. Filed 2026-08-12: the fix is now covered by a scheduled vector, and covering it found a residual

### 12.1 The coverage, and why it needed no `--add-opens`

§8's A/B is a hand-run against probes that are not checked in, and `probes/` is
never executed by `regression-suite/run.sh` in any suite. So this half now has a
scheduled assertion instead:
`regression-suite/src/RJdkExecutors.java::threadExitCleanup()`, in
`JDKONLY_CLASSES`.

The device is that `clearReferences()`'s one observable write is reachable from
**public API** — `Thread.getUncaughtExceptionHandler()` reads the same
`uncaughtExceptionHandler` field and falls back to the `ThreadGroup` when it is
null — so nothing here needs reflection, `--add-opens`, `StructuredTaskScope` or
a `ThreadFlock`. Three assertions, in this order because each is meaningless
without the one before it:

1. a live thread reports the handler installed on it (the field is populated and
   readable at all — the check §2's first draft was missing, which is how that
   draft reported the opposite answer);
2. the handler **fires** on an uncaught exception (so the dispatch consulted it
   while it was still installed, which is §5's forced ordering, observed);
3. after `join()` returns, the terminated thread no longer reports it.

(3) is the discriminator and it fails on the pre-fix behaviour: without
`Thread.exit()` the field is never nulled and the terminated thread hands the
handler straight back. It is asserted on the **abnormal** path — a thread whose
`run()` throws — for §6's reason, and because that is the path a cleanup wired
only into the happy path would miss.

Two things the vector deliberately does **not** assert, each because the answer
is not mode-independent:

* **The clean-death thread's handler.** See §12.2 — the answer differs between
  `--real-jdk` and `--jdk-only`, so pinning it would freeze one mode's
  divergence into a gate. The clean thread is still started, joined, and checked
  for "ran once", "did not dispatch a handler" and "cannot be restarted", which
  is everything about that path that IS mode-independent.
* **The exact handler-fire count.** CratonVM dispatches through the side table
  *and* lets real `Thread.dispatchUncaughtException` bytecode run on some paths,
  so the count is a per-mode fact. The vector asserts `>= 1` for the abnormal
  thread and then asserts the count is **unchanged** across the clean thread,
  which is the same question asked as a delta and is mode-independent.

### 12.2 Residual: the handler side table outlives `Thread.exit()`

**Found while writing §12.1, and it corrects §9's point 4.** §9 said "The VM's
own dispatch runs strictly before this point and consults a side table, so it is
unaffected". That is true of the **dispatch** and false of the **getter**.

`native-builtins/src/uncaught_handlers.rs` keeps per-thread handlers in an
identity-hash-keyed side table *and* mirrors them into the real
`Thread.uncaughtExceptionHandler` field, and its
`getUncaughtExceptionHandler()Ljava/lang/Thread$UncaughtExceptionHandler;` native
reads **the side table first**, falling back to the real field only when the
table misses. The table is emptied in exactly two places: `take_uncaught_handler`
(the abnormal dispatch path) and `clear_handler` (a setter called with null).
**Neither runs on a clean death.** `Thread.exit()` nulls the real field, so the
two stores now disagree for every normally-terminated thread that had a handler,
and the getter prefers the stale one.

Which answer you get depends on the mode, and the mechanism is the ambient
`NativeKind`:

| mode | who serves `getUncaughtExceptionHandler()` | terminated thread, clean death |
|---|---|---|
| `Compatible` / `--real-jdk` | the `Bridge` native wins | the **stale handler** (HotSpot: the ThreadGroup, or null) |
| `--jdk-only` | the native **yields** — `policy.is_jdk_only() && bytecode_available && kind != Intrinsic` — and real bytecode runs | the nulled field, i.e. HotSpot's answer |

Registrar and ambient kind, established from the call sequence rather than from
brace-scanning: `uncaught_handlers::register_uncaught_handler_natives` sets
ambient `NativeKind::Bridge` for all four `java/lang/Thread` triples and is
called three times — from `register_essential_natives` (`lib.rs`, the real-JDK
boot path), from `phases_late.rs`, and from `phases_late/concurrent.rs` — always
with the *same* callbacks, so last-write-wins is a no-op between them and this
registrar is the unambiguous winner on every path.

**Not fixed here, and the reason is not effort.** The obvious repair — make the
getter consult the real field first whenever the receiver's class *declares*
`uncaughtExceptionHandler` (asked of the class, not of the value:
`resolve_field_index_by_class_id` / `field_read::declares_field`, per
docs/architecture/natives-over-real-jdk-classes.md §4) — inverts a priority the
file documents as deliberate ("the side-table lookups intentionally keep priority
over the real field: they are the only storage that survives a synthetic Thread
layout"), and it is a **`Compatible`-mode behaviour change** on the
uncaught-exception path. That is exactly the population this directory's standing
constraints say to be most careful with, it is a HotSpot-parity fix rather than a
strict-mode one, and it cannot be validated without a run. The cheaper and
strictly safer alternative is to clear the table on a clean death too — one
`clear_handler` call from the normal branch of the worker-death paths in
`vm/src/vm/vm_exec.rs` — which converges both modes with no priority inversion,
but that is a third file again.

### 12.3 Correction 2026-08-12 (later pass): "converges both modes" is too strong

Re-read of `native-builtins/src/uncaught_handlers.rs` against today's tree. The
residual reproduces exactly — `clear_handler` is **private** and has one caller,
the null-setter arm at `:240`; `take_uncaught_handler` has one caller in the
whole tree, `vm/src/vm/vm_exec.rs:4804`, inside
`dispatch_uncaught_exception_shared`, i.e. the abnormal path only. So a clean
death leaves the entry, and the getter's short-circuit is
`get_uncaught_handler(...).or_else(|| real_thread_handler(...))` — side table
first, exactly as §12.2 says.

But §12.2's "cheaper and strictly safer alternative … converges both modes"
overstates what clearing the table buys. The registered
`getUncaughtExceptionHandler` native has **no `ThreadGroup` fallback at all**:
after the side table and the real field it tries `default_uncaught_handler` then
`real_default_handler` and otherwise answers `null`. HotSpot answers
`uncaughtExceptionHandler != null ? it : group`. So clearing the table converges
the *stale-versus-nulled* disagreement — which is the whole of this record's
residual — and leaves a **second, pre-existing** divergence untouched: in
`Compatible`/`--real-jdk` a thread with no per-instance handler answers `null`
where HotSpot answers its `ThreadGroup`, terminated or not. That one is not this
record's, it is not created by clearing the table, and it should not be smuggled
into the same edit. The comment on the fix should say so, so the next reader does
not measure the group case and conclude the clear did not work.

## 13. Filed 2026-08-12: the vector's restart assertion FAILED, and the state it must read is not the one `Thread.start()` reads

§12.1's vector ended with a fourth assertion — *"restarting a terminated thread
must throw `IllegalThreadStateException`"* — added because running Java teardown
on a dying thread is the change that could plausibly resurrect one. Its first
ever run failed. The defect it found is **older than this record's patch** and
independent of it: CratonVM has never enforced JVMS's start-at-most-once rule.

    cratonvm --java-home <jdk25> --jdk-only -cp regression-suite/build RJdkExecutors
    => AssertionError: restarting a terminated thread must throw IllegalThreadStateException
    HotSpot 25.0.3.9: PASS RJdkExecutors

### 13.1 Measured: the exception is the symptom, the resurrection is the damage

A standalone probe (`Thread` started, joined, started again) answers on the two
VMs:

| | HotSpot 25 | CratonVM `--jdk-only` |
|---|---|---|
| second `start()` throws | **yes** | no |
| body ran (`ran`) after the second `start()` | 1 | **2** |
| `start()` on a still-RUNNING thread throws | **yes** | no |

So a `Runnable` the application had already retired executes a second time on a
second OS thread. Both halves of the rule are missing, not just the terminated
one — which is why §12.1's vector now asserts the live half too, and asserts
`ran == 1` after the refused restart rather than only the exception.

### 13.2 Which lifecycle state is authoritative, and the one that is inert

Three stores model thread lifecycle in this VM. Only one of them is written on
the death path, and it is **not** the one the JDK's own `Thread.start()` reads.

Read with `--add-opens java.base/java.lang=ALL-UNNAMED` so `holder.threadStatus`
is reachable from Java, across one thread's whole life:

| store | NEW | after it has terminated | verdict |
|---|---|---|---|
| `Thread.holder.threadStatus` (what real `start()` branches on) | 0 / 0 | HotSpot **2**, CratonVM **0** | **INERT on this VM** |
| VM `ThreadRegistry` (`thread_run_state`) | NEW | **TERMINATED** | **authoritative** |
| `Thread.getState()` | NEW / NEW | TERMINATED / **TERMINATED** | correct *because* it bypasses the field |

`native_thread_get_state` says so in its own doc comment — *"the VM never
advances that field past 0 (NEW)"* — and computes the state from the registry
instead. `vm/src/vm/vm_exec.rs::thread_run_state` is explicit about why the
registry can answer for a dead thread: *"the registry RETAINS dead threads'
entries (`mark_dead` only flips `alive`), so a present-but-not-alive entry is
TERMINATED, while a missing entry is a thread that was never started (NEW)."*

**A guard placed on `holder.threadStatus` would therefore never fire.** That is
the whole trap in this defect: the field is the one the JVMS text and the JDK
source both point at, and it is the one thing here that nothing writes.

### 13.3 Why the real `start()`'s own check never runs

The JDK's `Thread.start()` bytecode *does* carry `if (holder.threadStatus != 0)
throw new IllegalThreadStateException()`. It is shadowed. `java/lang/Thread` /
`start` / `()V` is registered **twice** in `native-builtins/src/lib.rs` — once in
the real-JDK set and once in the synthetic set — and both land on
`native_thread_start0`. The real-JDK registration's own comment states the
intent and then contradicts itself in the next five lines:

> `Thread.start`: in real-JDK mode the JDK's Java implementation must run (it
> sets thread state, checks already-started, and calls start0). Only intercept
> for synthetic-JDK Thread …

— after which **both** branches call `native_thread_start0`. So the check is
bypassed *and* the field it reads is dead. Two independent reasons, either one
sufficient; fixing only the registration would still not throw, because
`threadStatus` stays 0.

Note the container route reaches the same place:
`shared_secrets_bridge.rs::jla_start_in_container` invokes
`Thread.start(Ljdk/internal/vm/ThreadContainer;)V`, whose real bytecode does
**not** re-check `threadStatus` (the public no-arg `start()` does that before
calling it) and ends in `start0()`. `start0()V` is natively registered on both
sets too. One guard in `native_thread_start0` therefore covers all four
registrations and both routes — and, because that bytecode's `finally` calls
`container.onExit(this)` when `start0` throws, a refused start does not leak a
container registration.

### 13.4 The fix, and why it reads two different things per layout

Out-of-file (`native-builtins/src/lang_system.rs`, in
`native_thread_start0`, before any of the `InheritableThreadLocal` / CCL
bookkeeping — HotSpot throws before any side effect). Exact text is in
§13.6.

The predicate is HotSpot's (`threadStatus != 0`, i.e. *anything but NEW*),
sourced from the registry. But the registry lookup
(`resolve_thread_id_from_thread_obj`) has **two** routes and only one is
aliasing-proof:

* **Real-JDK mirror** (`tid` reads back as a `Long`): resolved through the
  process-unique Java `Thread.tid` index, or the `tid`-checked pointer walk.
  A dead thread's recycled mirror address cannot alias — this is precisely the
  repair made for the *DoHead engine-start `IllegalThreadStateException` flake*,
  named in `read_java_thread_tid`'s and
  `find_thread_id_by_thread_obj_tid_checked`'s doc comments.
* **Fabricated mirror** (no `tid` field at all — the synthetic
  `java/lang/Thread` declares `contextClassLoader`, `_f5`, `threadLocals`,
  `inheritableThreadLocals` and four anonymous slots, and no `tid`): falls back
  to the **unguarded** pointer walk `find_thread_id_by_thread_obj` *before* it
  reads the synthetic marker. Asking `thread_run_state` there would reintroduce
  the DoHead flake as a spurious `IllegalThreadStateException` on a *fresh*
  thread.

So the guard asks the layout question first with the existing
`has_real_jdk_thread_layout` helper, and for the fabricated layout reads the
marker that lives **on the mirror itself**: `vm_exec.rs::thread_start` writes
`Value::Long(registry tid)` into slot 2 of a compatibility-stub `Thread`, and
nothing else puts a `Long` there — the same convention
`resolve_thread_id_from_thread_obj`'s own last fallback uses. A value on the
object cannot alias a side table keyed by address.

### 13.5 Blast radius of the narrowing — and why it is not a new one

`start()` now throws where it used to silently spawn. What in the corpus could
start a thread twice?

* **`ThreadPoolExecutor` — the dangerous case, and it is already gated.**
  `javap -c java.util.concurrent.ThreadPoolExecutor` on Adoptium 25.0.3.9 shows
  `addWorker` calling `Thread.getState()` and throwing
  `IllegalThreadStateException` when the answer is not `NEW`. That bytecode runs
  in `--jdk-only`, and `getState()` is the registry-backed native. **Every
  pooled worker in the corpus is therefore already gated on the exact predicate
  this guard adds**, off the exact same read. The guard adds no new
  false-positive surface in real-JDK mode; if the registry misresolved, the
  corpus would already be throwing from `addWorker`.
* **Misresolution of a fresh mirror** (the only way to get a *false* throw).
  Probe: 60 waves × 40 freshly constructed `Thread`s, each asked `getState()` /
  `isAlive()` before `start()`, with 2000-object heap churn per wave to force
  mirror-address recycling. **0 / 2400 misresolved**, in `--jdk-only` and in the
  default real-JDK mode. The synthetic mode could not be measured — the frozen
  wave binary is built without the `synthetic-jdk` feature and refuses
  `--synthetic-jdk` — which is the second reason the fabricated layout gets the
  on-mirror marker rather than the registry.
* **The eight natives that call `ctx.thread_start(...)` directly** —
  `jdk25_concurrency.rs:934`, `net_phase_e.rs:16503`, `lib.rs:22621/22659/22676`,
  `phases_late/concurrent.rs:4168/4231` — bypass `native_thread_start0` entirely
  and are **unaffected**. Each spawns a freshly allocated worker mirror. This is
  the argument for putting the guard in the native rather than in
  `vm_exec.rs::thread_start`: the Java-visible `Thread.start()` surface gets the
  JVMS rule, VM-internal spawns keep their current behaviour.
* **Not closed by this**: two threads calling `start()` on the same `Thread`
  concurrently. HotSpot's `start()` is `synchronized (this)`; this native is not,
  so a genuine double-start race narrows but does not vanish. Pre-existing, and
  out of scope here.

### 13.6 The patch

`native-builtins/src/lang_system.rs`, inserted immediately after
`native_thread_start0`'s `let this = match args.first() { … };` (before the
`Round-7 CRIT fix #3` comment):

```rust
    let already_started = if crate::has_real_jdk_thread_layout(ctx, this) {
        ctx.thread_run_state(this) != 0
    } else {
        ctx.object_num_fields(this) > 2
            && matches!(ctx.get_field(this, 2), Value::Long(_))
    };
    if already_started {
        return Err(RuntimeError::IllegalThreadStateException {
            message: "Thread.start: this thread has already been started".to_string(),
        }
        .into());
    }
```

Everything it names already exists: `has_real_jdk_thread_layout` is a
crate-root-private `fn` in `lib.rs` (called the same way, with a
`&mut dyn NativeContext`, at eight sites there); `thread_run_state`,
`object_num_fields` and `get_field` are `NativeContext` trait methods;
`RuntimeError` is already imported at the top of `lang_system.rs` and
`RuntimeError::IllegalThreadStateException { message }` already maps to
`java/lang/IllegalThreadStateException` in `types/src/error.rs`. A real-JDK
mirror whose `tid` does not read back as a `Long` takes the else-arm, reads the
real slot 2 (`name`, a reference), and fails **open** — never a spurious throw.

## What is not claimed

Nothing was rebuilt. §2's baseline, §3's pre-flight and §7's `ThreadLocal`
finding are measurements of HotSpot 25 and of the **pre-change** binary. They
establish that `Thread.exit()` did not run, that its body runs correctly on this
VM when it is reached, and that the `TerminatingThreadLocal` gate is closed for
a second reason. They do **not** establish that the edited source compiles, that
the two new call sites are reached, or that the pair works together — §8 is the
run that decides that, and it has not been done.

The same applies to §13, with one difference in its favour: §13.1's two-VM
divergence, §13.2's `threadStatus` time series and §13.5's 0/2400 misresolution
count are measurements of the **frozen wave binary**, so the defect and the
inertness of `holder.threadStatus` are facts, not hypotheses. §13.6's patch is
**SUPERSEDED 2026-08-12 (later pass): APPLIED verbatim at
`native-builtins/src/lang_system.rs:1048-1058` in commit `0113f2daf`, though
still not compiled or run by any lane that touched this record. The rest of this
paragraph is the original argument, kept for its shape.** It was written against
read signatures, and the `--synthetic-jdk` arm of §13.4 could not be exercised at
all because the frozen binary is built without that feature.
`RJdkExecutors.threadExitCleanup()` gained four checks for this (69 on HotSpot,
was 65); the transcript quoted in §13 predates the patch, so "CratonVM still
stops at the same first one" describes the pre-patch binary and says nothing
about today's source. Re-running that vector is the cheapest thing the next lane
can do with this record.
