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
>   never been exercised; and the container half is still interlocked off at
>   `shared_secrets_bridge.rs:745`
>   (`VM_REMOVES_THREADS_FROM_CONTAINERS: bool = false`). With this half landed,
>   the ordering hazard W7-23 documented is now resolvable — flip the interlock
>   and measure the pair.

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

### C. The main thread does not get `exit()` either

`Thread.exit()` is wired into the two worker death paths in
`vm/src/vm/vm_exec.rs`. HotSpot's `JavaThread::exit` runs on the primordial
thread too, at VM shutdown. CratonVM's main-thread teardown is in a different
file (the one that owns the other `clear_tlab_addr` call site) and is out of
this lane's ownership, so it is recorded rather than changed. It matters much
less — the process is ending, so no observer survives to see a container that
was not emptied — and running arbitrary Java during VM shutdown is a materially
riskier proposition than running it on a worker. Whoever takes it should measure
`ExitBodyProbe`-on-main first, which already passes (§3 invokes `exit()` on the
main thread and the VM stays usable).

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

## What is not claimed

Nothing was rebuilt. §2's baseline, §3's pre-flight and §7's `ThreadLocal`
finding are measurements of HotSpot 25 and of the **pre-change** binary. They
establish that `Thread.exit()` did not run, that its body runs correctly on this
VM when it is reached, and that the `TerminatingThreadLocal` gate is closed for
a second reason. They do **not** establish that the edited source compiles, that
the two new call sites are reached, or that the pair works together — §8 is the
run that decides that, and it has not been done.
