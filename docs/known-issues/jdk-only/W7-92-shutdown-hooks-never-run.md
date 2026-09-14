# W7-92 — `Runtime.addShutdownHook` registers, and nothing ever starts the thread

> ## RECONCILED 2026-08-12 (lane C18) — the DEFECT is measured; the FIX is not
>
> Keep these two apart, because this record's hedges cover both and only one of
> them is still open.
>
> * **The defect is MEASURED, and stated flatly: shutdown hooks NEVER RUN.**
>   Against HotSpot 25 on the same program, HotSpot prints **three hook lines**
>   and CratonVM prints **none**, **with no output-lost marker**. That last
>   clause is the one that matters: §0's warning that *"a hook that runs but
>   whose output is lost is indistinguishable from a hook that never ran"* is
>   **discharged** — this is not an output-capture artefact. Any record that
>   still hedges on *whether* hooks run should be read against this.
> * **The fix is NOT measured.** Every CratonVM "after" in this record stays
>   **PREDICTED**, and `ShutdownProbe` across the nine exit modes
>   (`WAVE-D-QUEUE.md`, measurement 3) is still owed. Nothing here has been
>   upgraded.
> * **§0's "12, 9 and 36 findings" are superseded as counts.** The
>   re-adjudication is `C8-CORPUS-HARNESS-DEFECTS-20260812.md` §4.1: **62 of
>   75 stored `DIVERGE` rows (83%) were harness artefacts.** §0's *argument* —
>   one missing feature wearing many hats — is confirmed, not weakened; only
>   the arithmetic moved.

Status: **FIX LANDED (lane C11, 2026-08-12), CratonVM side NOT YET RUN.** The
runner exists, each hook is `start()`ed as its own thread and joined, and every
exit path but `Runtime.halt` reaches it — see §7 for the implementation and §8
for the four contract questions §1 left open, now measured. Every CratonVM
"after" in this record is **PREDICTED** until the orchestrator runs
`ShutdownProbe` and `RShutdownHooks` against a build of it. Two residual
divergences are stated in §9 and are why this doc stays in `known-issues`.

Diagnosis (§0–§6) by lane C9, 2026-08-12: the registry is correct and complete;
**execution was absent on every one of the five exit paths.** Measured against
HotSpot 25.0.3+9 on all five, plus `Runtime.halt`, `removeShutdownHook`,
concurrent hooks and duplicate registration.

Predecessors, all of which name this gap and none of which closes it:
`P4A-SPRING-20260812.md` §7 N1 (the nomination this record completes and
corrects), `P4A-TOMCAT-20260812.md` §5, `P4A-CORPORA-20260812.md` §1 and §7,
`W7-27-thread-exit-java-cleanup.md` §10 ("do not let it ride along"),
`W5-1-loadlibrary-allowlist-too-wide.md` §357.

---

## 0. Why this one is worth a record and not a line

**This gap has already caused measurement error three times, and the error was
sweeping rather than local.** A corpus harness key included a `completed=`
marker printed from a shutdown hook. The hook never fired, so the marker never
appeared, so the key never matched — and three separate lanes each read that as
a cross-VM **DIVERGE** verdict against the VM, at 12, 9 and 36 findings
respectively. One missing feature wore fifty-seven hats.

The specific reason it was so expensive is worth stating on its own, because it
generalises: **a hook that runs but whose output is lost is indistinguishable
from a hook that never ran.** Both produce "the marker is absent". Every probe
and vector in this record is built to separate those two states, and that
separation is the most valuable thing here — more valuable than the fix, because
the fix can be got wrong in exactly the way that reproduces the ambiguity.

---

## 1. The HotSpot oracle — all five exit paths, plus four contract questions

`ShutdownProbe.java` (§6), one mode per exit path. Every mode prints
`MAIN-END <mode>` **from `main`** before leaving, so "the hook did not run" and
"the program never got there" are different observations. The hook reports on
three channels: `System.out`, `System.err`, and a raw
`FileOutputStream(FileDescriptor.out)` + `flush()` that bypasses `System.out`.

Measured, `openjdk 25.0.3 2026-04-21 LTS` / `OpenJDK 64-Bit Server VM
Microsoft-13877124 (build 25.0.3+9-LTS, mixed mode, sharing)`, Windows 11:

| mode | exit path | hook ran? | `startedAsThread` | rc |
| --- | --- | --- | --- | --- |
| `normal` | `main` returns | **yes**, all 3 channels | `true` | 0 |
| `exitmain` | `System.exit(3)` from `main` | **yes**, all 3 | `true` | 3 |
| `exitother` | `System.exit(4)` from a non-main thread while `main` sleeps | **yes**, all 3 | `true` | 4 |
| `uncaught` | `IllegalStateException` out of `main` | **yes**, all 3, AFTER the stack trace | `true` | 1 |
| `nondaemon` | `main` returns, a non-daemon thread runs on | **yes**, all 3, AFTER `KEEPER-DONE` | `true` | 0 |
| `halt` | `Runtime.halt(5)` | **NO** — correctly skipped | — | 5 |
| `remove` | `removeShutdownHook` then return | **NO** | — | 0 |
| `multi` | four hooks | **all four ran** | `true` ×4 | 0 |
| `dup` | re-register the same `Thread` | — | — | 0 |

Verbatim, the two that carry the most information:

```
=== MODE uncaught ===
MAIN-START uncaught
REGISTERED uncaught alive=false state=NEW
MAIN-END uncaught
Exception in thread "main" java.lang.IllegalStateException: deliberate-uncaught
	at ShutdownProbe.main(ShutdownProbe.java:86)
HOOK-RAN-OUT uncaught thread=hook-uncaught startedAsThread=true
HOOK-RAN-ERR uncaught
HOOK-RAN-FD1 uncaught
rc=1

=== MODE halt ===
MAIN-START halt
REGISTERED halt alive=false state=NEW
MAIN-END halt
rc=5
```

Four contract answers that a fix must not get wrong, all measured rather than
recalled:

1. **`removeShutdownHook` returns `true` once and `false` after.**
   `REMOVED=true` / `REMOVED-AGAIN=false`.
2. **A registered hook is `NEW`, not started.** `alive=false state=NEW`.
3. **Re-registering the same `Thread` throws `IllegalArgumentException`** —
   `DUP-IAE java.lang.IllegalArgumentException`.
4. **A thread that has already run to completion IS accepted.** The oracle
   printed `STARTED-ACCEPTED`. `ApplicationShutdownHooks.add` rejects on
   `isAlive()`, and a terminated thread is not alive. This corrects the probe's
   own first draft, which expected an IAE and labelled the real answer
   "(wrong)". **A probe's setup is code that can be wrong**; the oracle is what
   settles it.

`multi` also shows the ordering guarantee HotSpot actually gives: the four hooks
each ran, and their relative order is not specified (they are started
concurrently and joined). A fix must therefore not be judged on hook ORDER.

---

## 2. What CratonVM does — the mechanism, at both ends

### 2.1 The registry is a write-only list

`native-builtins/src/lang_system.rs:55`:

```rust
static SHUTDOWN_HOOKS: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());
```

Three references in the whole tree, all in that file:
`shutdown_hook_add` pushes, `shutdown_hook_remove` pops. **There is no reader
that runs anything.** This is the "write-only counter" shape: grep the
subsystem's statics for one with two hits and no consumer, and the diagnosis is
already there. The file's own comment says so in place, at line 50:

> Known gap, unchanged by this registry: cratonvm does not yet RUN the
> registered hooks at VM shutdown.

The rooting half is correct and must be preserved — a hook is normally never
started, so on HotSpot the only thing keeping it and its whole object graph
alive is `ApplicationShutdownHooks.hooks`, and WildFly's MSC leak detector fired
mid-boot when the hook was dropped on the floor. **The defect is the missing
reader, not the registry.**

### 2.2 In `--real-jdk` mode the JDK's own machinery is cut off at BOTH ends

This is the part no predecessor record states, and it changes what a fix should
look like. From `--dump-native-registry` (5.9 MB dump, `p1/reg.json`), the two
rows:

```json
{"class":"java/lang/Runtime","name":"addShutdownHook","descriptor":"(Ljava/lang/Thread;)V",
 "kind":"bridge","registered_by":"native-builtins/src/lang_system.rs:1552",
 "overwrote":null,"invocations":0,"owns_slot":true,
 "real_declaring_method":{"loaded":true,"declared":true,"acc_native":false,"has_code":true}}
{"class":"java/lang/Runtime","name":"removeShutdownHook","descriptor":"(Ljava/lang/Thread;)Z",
 "kind":"bridge","registered_by":"native-builtins/src/lang_system.rs:1564",
 "overwrote":null,"invocations":0,"owns_slot":true,
 "real_declaring_method":{"loaded":true,"declared":true,"acc_native":false,"has_code":true}}
```

`acc_native: false, has_code: true` — in the real JDK these are ordinary Java
methods whose bodies call `ApplicationShutdownHooks.add/remove`. CratonVM
registers a native **over** them and `owns_slot: true`, so:

* **the registration end** — `ApplicationShutdownHooks.hooks` is never
  populated, because its `add` never runs; and
* **the execution end** — `java/lang/System.exit(I)V` and
  `java/lang/Runtime.exit(I)V` are likewise intercepted (also
  `acc_native:false, has_code:true`), so `Shutdown.exit` → `runHooks()` is never
  reached either.

`java/lang/Shutdown.runHooks` is registered nowhere and invoked nowhere. The
only `java/lang/Shutdown` triples that exist are `beforeHalt` and `halt0`, both
registered in `lang_system.rs:1592/1596` for the `Runtime.halt` path.

So a fix has two shapes available, and the cheap one is not obviously the wrong
one:

* **(A) run them Rust-side** from `SHUTDOWN_HOOKS`. Small, mode-independent,
  works in synthetic-JDK mode too — where there is no `ApplicationShutdownHooks`
  at all. Diverges from HotSpot in one observable way if the hooks are run
  inline (see §4).
* **(B) stop intercepting in `--real-jdk` mode** and let the JDK's own
  `ApplicationShutdownHooks` / `Shutdown.exit` / `runHooks` do it. Highest
  fidelity — it starts real threads and joins them — but it removes two
  interceptions and reroutes `System.exit`, on a VM where `Runtime.exit`'s
  interception was itself a correctness fix eight lines of comment long
  (`lib.rs:14465-14483`, W7-86). Not a one-wave change.

**Take (A) first and record (B) as the target.** Do not take both in one wave.

### 2.3 The five exit paths, and the three distinct code sites

| exit path | where CratonVM terminates | runs hooks today |
| --- | --- | --- |
| `main` returns | `vm-cli/src/main.rs`, after the non-daemon join, `match result { Ok(_) => Ok(()) }` (~`:4667`) | no |
| `System.exit(n)` from `main` | `lang_system.rs:1482` `std::process::exit(code)` | no |
| `System.exit(n)` from another thread | the **same** native, on that thread's ctx | no |
| uncaught exception out of `main` | `vm-cli/src/main.rs:4672` `Err(ExceptionThrown)` → `bail!` | no |
| non-daemon thread outlives `main` | `wait_for_non_daemon_threads(None)` at `:4660`, then the row above | no |
| `Runtime.halt(n)` | `native_shutdown_halt0`, `lang_system.rs:2792` | no — **and this one is CORRECT** |

Note the shape: the two `exit` rows are one function, so a fix there buys two
paths. The two launcher rows are one file. **`Runtime.halt` must stay as it is**,
and the asymmetry has to be written down or a later tidying sweep will "fix" it.

---

## 3. NOMINATIONS

Lane C9 owns `native-builtins/src/lib.rs` and this record. The fix does not live
in `lib.rs` — `SHUTDOWN_HOOKS` and all three exit natives are in
`lang_system.rs` — so everything below is a nomination.

`P4A-SPRING-20260812.md` §7 N1 already specifies **7.1 through 7.4** (the
`run_shutdown_hooks` runner and its two `exit` call sites) with exact literal
old/new text, and that text still applies verbatim: verified against
`origin/dev` and against this working tree, `run_shutdown_hooks` exists nowhere
and `git log -S"run_shutdown_hooks"` is empty. **Do not re-derive it. Apply N1
7.1–7.4 as written, then apply the three corrections below.**

### N1-C1 — N1's runner must not report success it did not have

N1's body logs a `tracing::warn!` when a hook throws and otherwise says nothing.
That leaves "ran three hooks" and "the list was empty" identical in the output,
which is the ambiguity this record is about, sitting inside its own repair —
the third time this campaign has caught that reflex (W6-5 §3.4, W7-51 §3, here).
Add one unconditional line **after** the loop in `run_shutdown_hooks`:

```rust
    eprintln!("[cratonvm] shutdown hooks: ran={ran} threw={threw} trigger={trigger}");
```

with `ran`/`threw` counted in the loop and `trigger: &str` added as a second
parameter (`"System.exit"`, `"Runtime.exit"`, `"main-returned"`,
`"uncaught"`). Unconditional, on stderr, next to the `[cratonvm] System.exit(N)
called` line that is already unconditional there. `ran=0` on a program with no
hooks is the honest reading and is what makes `ran=0` on a program WITH hooks a
finding.

### N1-C2 — 7.5's missing bridge, made concrete

N1 §7.5 correctly says the launcher paths need it and deliberately leaves the
bridge unwritten because `run_shutdown_hooks` takes `&mut dyn NativeContext` and
the launcher holds a `Vm`. The bridge that needs no new plumbing:

**(a)** in `native-builtins/src/lang_system.rs`, next to the existing
`java/lang/Shutdown` registrations at `:1592`, register the JDK's own
`runHooks` triple onto the new runner:

```rust
    registry.register_with_kind("java/lang/Shutdown", "runHooks", "()V", |ctx, _args| {
        run_shutdown_hooks(ctx, "Shutdown.runHooks");
        Ok(None)
    }, NativeKind::Bridge);
```

`java.lang.Shutdown.runHooks()` is `private static void` with a `Code`
attribute in the real JDK, so this is an interception of the same kind as the
two `exit` natives — and it is the *right* interception while the hooks live
Rust-side, because the JDK's `Shutdown.hooks` array is empty by §2.2.

**(b)** in `vm-cli/src/main.rs`, on the `Ok` path, after the non-daemon join and
**above** the `match result`, i.e. immediately before line 4667's
`match result {`:

```rust
    // W7-92: hooks on the normal-return and uncaught-throw paths. ABOVE the
    // `match` for the same reason the slot-map census at :4361 is: a workload
    // that threw out of `main` still runs its hooks on HotSpot (measured —
    // see the record's `uncaught` row).
    let _ = vm.invoke("java/lang/Shutdown", "runHooks", "()V", &[]);
```

`vm.invoke(class, name, descriptor, args)` is the launcher's existing Java entry
point — it is how `main` itself is called at `:4319`. In synthetic-JDK mode
`java/lang/Shutdown` may not resolve; `let _ =` is deliberate and the
unconditional `ran=` line from N1-C1 will not print, which is the honest signal
that the bridge did not fire.

**Ordering, and it is not cosmetic:** this must go AFTER
`wait_for_non_daemon_threads`, because HotSpot's `nondaemon` transcript prints
`KEEPER-DONE` before the hook. Shutdown does not begin until the last non-daemon
thread ends.

### N1-C3 — the `dup` and already-terminated contracts

`shutdown_hook_add` currently returns silently when the same hook is registered
twice; its doc comment calls that "the conservative choice". HotSpot throws
`IllegalArgumentException` (§1.3, measured). While the list was never read this
was invisible; once hooks run, a program that relies on the throw to detect
double-registration gets a second silent no-op. Small, separable, and listed
here so it is not discovered later as a regression of the fix:

*old* (`lang_system.rs`, in `shutdown_hook_add`):
```rust
        {
            return;
        }
```
*new*:
```rust
        {
            // HotSpot: `ApplicationShutdownHooks.add` throws
            // IllegalArgumentException("Hook previously registered").
            // Measured on 25.0.3+9; see W7-92 §1.3.
            return Err(RuntimeError::IllegalArgumentException {
                message: "Hook previously registered".to_string(),
            }
            .into());
        }
```
which requires `shutdown_hook_add` to return `Result<(), MethodCallFailed>` and
its one caller at `:1556` to `?` it. **Do NOT also reject an already-terminated
thread** — §1.4 measured HotSpot accepting one.

### N2 — schedule the vector

`regression-suite/run.sh:106`, `CORE_CLASSES`. It is a `--real-jdk`
compatibility behaviour, so `CORE_CLASSES` and deliberately **not**
`JDKONLY_CLASSES`. Append `RShutdownHooks` to the end of the `CORE_CLASSES`
string:

*old*:
```
 RJdkFormatLocale RJdkStrictMath RJdkByteOrder RJdkIntrinsics"
```
*new*:
```
 RJdkFormatLocale RJdkStrictMath RJdkByteOrder RJdkIntrinsics RShutdownHooks"
```

`run.sh`'s coverage census counts `CORE + JDK-only + unregistered == the number
of sources`, and `STRICT_COVERAGE=1` is now set in CI — so landing
`regression-suite/src/RShutdownHooks.java` **without** this line turns the
census red. The source and the registration must land together.

---

## 4. What a fix must not silently get wrong

1. **`Runtime.halt` must keep skipping hooks.** `native_shutdown_halt0`'s doc
   comment already states the rule; N1 §7.4 restates it. The `halt` row of §1 is
   the measurement that backs it.
2. **Inline `run()` is not `start()`.** N1's runner calls
   `ctx.invoke_virtual(hook, "run", "()V", &[])` on the exiting thread. HotSpot
   starts each hook as its own thread and joins them all. The difference is
   observable three ways: `Thread.currentThread().getName()` inside the hook
   (which is why the probe and the vector both print `ownThread`), a hook that
   blocks on another hook (inline runs deadlock where HotSpot does not), and a
   hook that reads a `ThreadLocal`. This is an acceptable first step and an
   unacceptable silent one — the vector asserts `ownThread=true`, so an inline
   fix reports a **remaining** diff rather than a green.
3. **Re-entrancy.** A hook that itself calls `System.exit` re-enters the native.
   N1's `std::mem::take` drain handles it (the second pass sees an empty list),
   but the nested `std::process::exit` still fires from inside the hook, where
   HotSpot's `Shutdown.exit` is `synchronized` and the nested call blocks
   forever. Worth a comment; not worth a redesign.
4. **The output channel.** If hooks run but `System.out` is dead or unflushed at
   that point, the observable is identical to "never ran" and this record's own
   headline error repeats. The vector's `hookFd1` line, written straight to fd 1
   and flushed, exists solely to make that case distinguishable. **Do not
   "simplify" it onto `System.out`.**
5. **`invocations: 0` in the dump is not proof of anything here.** The dump was
   taken from a workload that registers no hooks. What proves the slot is owned
   is `owns_slot: true` plus `real_declaring_method.has_code: true`.

---

## 5. Evidence, and what would falsify it

**Measured:** the entire §1 oracle table, on HotSpot 25.0.3+9, all nine modes,
transcripts in hand. The `RShutdownHooks` vector is green on HotSpot at 4 checks
and **mutation-checked four ways** — base, "registered but never run",
"a removed hook runs anyway", and "ran but `System.out` is dead" — producing
four distinct outputs after `run.sh`'s own `extract()` filter:

| arm | filtered output |
| --- | --- |
| base | `pre` + `PASS checks=4` + `hookOut` + `hookFd1 … out=ok` |
| never-runs | `pre` + `PASS checks=4` only |
| removed-hook-runs | base + `CK RShutdownHooks REMOVED-HOOK-RAN (must not appear)` |
| ran-but-stdout-dead | `pre` + `PASS checks=4` + `hookFd1 … out=LOST`, no `hookOut` |

The third and fourth rows are the ones worth having: without the negative arm a
VM that started every `Thread` it had ever seen would pass, and without the raw
channel the fourth state would read as the second.

**PREDICTED, not run — this lane could not execute the VM (a build was in
flight):** every CratonVM column. The prediction is that all four hook-bearing
modes print no `HOOK-RAN-*` line at all, on both `--real-jdk` and `--jdk-only`,
and that `halt` and `remove` agree with HotSpot trivially (by doing nothing for
the wrong reason — which is why they are not evidence of correctness). The
structural basis is strong rather than a judgement call: `SHUTDOWN_HOOKS` has no
reader, and that is a fact about the tree, not an opinion about a test.

**What would falsify this record:** a CratonVM run of `ShutdownProbe normal`
that prints `HOOK-RAN-FD1 normal`. One command settles it.

---

## 6. Probes

`ShutdownProbe.java` + `run-shutdown-probe.sh`, in lane C9's scratchpad
(`…/scratchpad/c9/`), not checked in. Nine modes, one per exit path plus the
four contract questions. `run-shutdown-probe.sh` runs the HotSpot arm alone by
default and adds `--real-jdk` and `--jdk-only` CratonVM arms when `CRATONVM` is
set. It captures each arm's output into a variable before printing, because
`cmd | tail` reports `tail`'s exit status and the rc is load-bearing here
(`exitmain` must be 3, `exitother` 4, `uncaught` 1, `halt` 5).

`regression-suite/src/RShutdownHooks.java` is the shippable half and is checked
in; it needs the N2 registration line to be scheduled.

`HookContract.java` and `RunTerm.java` (§8) are in lane C11's scratchpad
(`…/scratchpad/c11/`), not checked in. They answer the contract questions that
`ShutdownProbe` does not, and every one of them had to be settled before the
runner could be written — three of the four decided a branch in it.

---

## 7. The fix, as landed — lane C11, 2026-08-12

Shape **(A)** of §2.2: run them Rust-side from `SHUTDOWN_HOOKS`. Shape (B)
(stop intercepting and let the JDK's own `ApplicationShutdownHooks` do it)
remains the target and is still not a one-wave change.

Two files, both owned by the implementing lane:
`native-builtins/src/lang_system.rs` and `vm-cli/src/main.rs`.

### 7.1 The reader

`run_shutdown_hooks(ctx: &mut dyn NativeContext, trigger: &str)`, next to the
registry it reads. Reached from four places:

| exit path | call site | trigger |
| --- | --- | --- |
| `System.exit(n)`, from any thread | `native_system_exit`, before the slot-map sweep | `System.exit` |
| `Runtime.exit(n)` | `native_runtime_exit`, same position | `Runtime.exit` |
| a JDK-side route into shutdown | the new `java/lang/Shutdown.runHooks()V` native | `Shutdown.runHooks` |
| `main` returned / `main` threw | `vm-cli/src/main.rs`, after the non-daemon join, above `match result` | `main-returned` / `uncaught` |
| **`Runtime.halt(n)`** | **none, deliberately** | — |

Before the sweep, not after: a hook is Java code that loads classes and
allocates, so `sweep_declared_slot_maps_before_exit` would otherwise census a
heap the hooks are about to change.

**Hooks are started as threads, not `run()` inline.** §4.2 called inline
execution "an acceptable first step"; it was cheap enough to skip that step and
three separate measurements say inline would have been wrong rather than merely
approximate — the vector asserts `ownThread=true`, HotSpot's hooks are
concurrent (§8, `crosswait`), and a hook that touches anything requiring a stack
walk needs a real Java frame on a real VM thread. Two loops, start-all then
join-all, exactly as `ApplicationShutdownHooks.runHooks` has them; one
start-join pair per hook would deadlock the `crosswait` shape.

**The join is bounded, and this is the one deliberate divergence in the runner.**
HotSpot loops on `hook.join()` forever, so a hung hook hangs the JVM with no
diagnostic. This VM is read by harnesses that score a wedged process as "the VM
hung" and file it as a finding, which is the failure mode this record exists to
stop. Default bound 30 s, then a named line on stderr and exit continues;
`CRATONVM_SHUTDOWN_HOOK_TIMEOUT_MS=0` restores HotSpot's unbounded wait, any
other value sets the bound. The join polls `thread_is_alive` inside the
`begin_blocking_region` / `end_blocking_region` protocol — mandatory, not
hygiene: the hook threads allocate, and a thread sleeping outside a blocked
region never reaches the safepoint a collection needs.

Handles are re-resolved from the global-root table on **every** use rather than
cached as `ObjectRef`s across the invokes. The roots are GC-remapped; a cached
raw ref would go stale under the moving collector precisely because a hook body
allocates.

### 7.2 N1-C1, honoured — one unconditional line

```
[cratonvm] shutdown hooks: ran=1 threw=0 skipped=0 unjoined=0 trigger=main-returned
```

Unconditional, on stderr, beside the `[cratonvm] System.exit(N) called` line
that is already unconditional there. `ran=0` for a program with no hooks is the
honest reading and is what turns `ran=0` on a program *with* hooks into a
finding instead of a silence. Four counters rather than the two N1-C1 asked for,
because the runner can now distinguish four states:

* `ran` — started and observed to finish.
* `threw` — `start()` failed at the VM boundary. An exception from a hook's
  **body** is deliberately not counted: it lands on the hook's own thread and
  HotSpot swallows it (§8, `throwing` — rc unchanged, the other two hooks ran).
* `skipped` — already started, or already finished. HotSpot does not re-run a
  terminated hook either (§8, `RunTerm`).
* `unjoined` — started, still running when the bound expired.

`extract()` in `regression-suite/harness-guard.sh` keeps only `^(PASS|CK) `, so
this line is filtered out of every scheduled vector's diff and cannot itself
become a cross-VM difference.

### 7.3 N1-C2, honoured — and the bridge is NOT `vm.invoke`

Both halves land, but not as N1-C2 wrote them:

* `java/lang/Shutdown.runHooks()V` **is** registered onto the runner
  (`NativeKind::Bridge`), so any JDK-side route into shutdown reaches the same
  drain. It is the right interception while the hooks live Rust-side, because
  `ApplicationShutdownHooks.hooks` is never populated (§2.2) and running the
  real body would run nothing.
* the launcher **does not** go through it. `vm.invoke("java/lang/Shutdown",
  "runHooks", "()V", &[])` needs `java/lang/Shutdown` to resolve, which is a
  real-JDK-mode assumption, and N1-C2's own `let _ =` would have swallowed the
  failure — a bridge that silently does nothing in synthetic-JDK mode is the
  exact shape this record is about. The launcher builds a
  `cratonvm_vm::vm::NativeContextImpl { shared: &vm.shared, thread: &mut
  vm.main_thread }` (the same construction `Vm::begin_main_thread_blocking_
  region` uses ten lines earlier) and calls `run_shutdown_hooks` directly. Both
  routes drain the one list, so neither can double-run a hook.

Placement is N1-C2's: after `wait_for_non_daemon_threads`, above `match result`.
The ordering is load-bearing — HotSpot's `nondaemon` transcript prints
`KEEPER-DONE` before the hook output.

### 7.4 N1-C3, honoured, plus one contract N1 did not have

`shutdown_hook_add` now returns `Result<(), MethodCallFailed>`:

* duplicate registration → `IllegalArgumentException("Hook previously
  registered")` (§1.3);
* registration after shutdown has begun → `IllegalStateException("Shutdown in
  progress")` (§8, new). Without it a late registration is accepted and then
  silently dropped, which is this record's own defect reintroduced by its
  repair.

`shutdown_hook_remove` returns `Result<bool, MethodCallFailed>` and throws the
same ISE during shutdown, also measured. A hook thread that has already run to
completion is still **accepted** at registration (§1.4) and is `skipped` at
shutdown.

`native_thread_start0`'s "has this thread already been started" predicate moved
into `thread_already_started` in the same file and both callers now use it. The
runner has to ask the identical question, and two copies of it would be a twin
pair with nothing keeping them in step — the drift species this tree has a
record of.

### 7.5 PREDICTED CratonVM behaviour

Every row below is a prediction from the code, not a measurement; the lane could
not execute the binary.

| probe | predicted `--real-jdk` and `--jdk-only` |
| --- | --- |
| `ShutdownProbe normal` | `HOOK-RAN-OUT/-ERR/-FD1`, `startedAsThread=true`, rc 0 |
| `ShutdownProbe exitmain` | same, rc 3 |
| `ShutdownProbe exitother` | same, rc 4 |
| `ShutdownProbe uncaught` | hook output present, rc 1, but **before** the trace (§9.1) |
| `ShutdownProbe nondaemon` | hook output after `KEEPER-DONE`, rc 0 |
| `ShutdownProbe halt` | no hook output, rc 5 |
| `ShutdownProbe remove` / `dup` | unchanged; `dup` now throws IAE |
| `RShutdownHooks` | all four HotSpot lines, `ownThread=true` |
| `CorpusMain` | `CORPUS-END … completed=exit` finally appears — the marker whose absence produced the 12/9/36 false DIVERGE verdicts |

The most valuable falsifier is unchanged and is now inverted: a CratonVM run of
`ShutdownProbe normal` that prints **no** `HOOK-RAN-FD1 normal`, or a
`ran=0` on the `[cratonvm] shutdown hooks:` line while a hook was registered.

---

## 8. The four contract questions §1 left open — measured

HotSpot `25.0.3+9`, Windows 11, `HookContract.java` / `RunTerm.java`. Each one
decided a branch in §7's runner.

**(a) `addShutdownHook` / `removeShutdownHook` during shutdown → ISE.**

```
=== MODE addduring
MAIN-START addduring / MAIN-END addduring
ADD-DURING threw=java.lang.IllegalStateException msg=Shutdown in progress
rc=0

=== MODE removeduring
MAIN-START removeduring / MAIN-END removeduring
OTHER-HOOK-RAN
REMOVE-DURING threw=java.lang.IllegalStateException msg=Shutdown in progress
rc=0
```

Note the second one: the hook the late `remove` tried to cancel ran anyway.

**(b) A hook that throws does not stop the others and does not change the exit
code.**

```
=== MODE throwing
MAIN-END throwing
HOOK-3-RAN
HOOK-BOOM-ENTERED
Exception in thread "h-boom" java.lang.IllegalStateException: deliberate-hook-throw
	at HookContract.lambda$main$5(HookContract.java:70)
	at java.base/java.lang.Thread.run(Thread.java:1474)
HOOK-1-RAN
rc=0
```

All three ran; the trace is printed by the hook's own thread; `rc=0`. Note also
the order — 3, boom, 1 — which is the §1 warning about hook ORDER, measured.

**(c) Hooks run concurrently, and a hook may block on another hook.**

```
=== MODE crosswait
SIGNAL firing
WAITER released=true
rc=0
```

`h-waiter` blocks on a `CountDownLatch` that `h-signal` counts down. This is the
measurement that rules out an inline runner: inline, `h-waiter` would have burned
its 10 s timeout and answered `released=false`, or deadlocked outright with no
bound.

**(d) `System.exit` inside a hook hangs HotSpot forever; `Runtime.halt` inside a
hook terminates immediately.**

```
=== MODE exitinhook (bounded to 12s by the probe runner)
MAIN-END exitinhook
PEER-HOOK-RAN
EXIT-HOOK-ENTERED
rc=124        <- `timeout` fired; the JVM was still alive

=== MODE haltinhook
HALT-HOOK-ENTERED
rc=9
```

§4.3 predicted the hang from `Shutdown.exit` being `synchronized`; it is
confirmed. CratonVM does **not** reproduce it (§9.2).

**(e) A hook thread that already ran is accepted and is not re-run.**
`RunTerm`: `DEAD-BODY-RAN` appears once (from the explicit `start()`), then
`MAIN-END`, then the three live hooks — no second `DEAD-BODY-RAN`, no
`IllegalThreadStateException` reaching the console.

---

## 9. Residual divergences and NOMINATIONS

### 9.1 OPEN — on the uncaught path the hook output precedes the stack trace

HotSpot: trace, then hooks (§1, `uncaught`). CratonVM after this fix: hooks,
then trace. Not a placement mistake — the launcher renders a fatal exception by
`bail!`ing a joined string that `main()` prints **after** `run()` returns, so no
call site inside `run()` can be after it. Fixing it means restructuring how the
launcher renders a fatal exception, which changes output that every harness in
this tree reads; it is deliberately not folded into this wave. `rc` and the
presence of the hook output are both correct.

### 9.2 STATED — `System.exit` from inside a hook terminates instead of hanging

HotSpot deadlocks (§8d). Here the second entry finds the list drained, prints
`ran=0 … trigger=System.exit`, and `std::process::exit` fires from the hook
thread. That is a divergence in CratonVM's favour and it is not worth a
redesign, but it must be written down rather than discovered: a workload that
relies on the hang (there are none known) would see the process die.

### 9.3 NOMINATION — the signal door is still closed, and the pair must not
silently re-close

`native-builtins/src/lib.rs` (lane C9's file) carries a corrected comment on the
`jdk/internal/misc/Signal.handle0` silent accept, whose current text says a
Tomcat or log4j teardown "reaches this VM through neither door". **One of those
two doors is now open.** The comment ends with "do not restore the old sentence
until its `RShutdownHooks` vector is green", which is the right condition; the
nomination is to replace the middle of it once the orchestrator has run the
vector:

*old*:
```
    // It does not. `Runtime.addShutdownHook` is intercepted by a native in
    // `lang_system.rs` that roots the hook in a `SHUTDOWN_HOOKS` vector whose
    // only reader is `removeShutdownHook`, and no exit path in this VM runs
    // it — not `System.exit`, not `Runtime.exit`, not the launcher's
    // post-`main` return, not the uncaught-exception path. So a Tomcat or
    // log4j teardown reaches this VM through neither door: the signal
    // handler is accepted and never fires, AND the shutdown hook is accepted
    // and never runs. The accept above is still the right call for boot; what
    // is removed is the false consolation. See
```
*new*:
```
    // It was not true when it was written. W7-92 has since given
    // `SHUTDOWN_HOOKS` a reader (`lang_system::run_shutdown_hooks`, reached
    // from `System.exit`, `Runtime.exit`, `Shutdown.runHooks` and the
    // launcher's post-`main` path), so the addShutdownHook door IS now open
    // and a Tomcat or log4j teardown registered that way does run. The
    // SIGNAL door is still shut: this handler is accepted and never fires,
    // so a teardown that depends on SIGTERM/SIGINT — not on a registered
    // hook — still gets nothing, and nothing in this VM converts a signal
    // into a call to `run_shutdown_hooks`. That is the remaining half. See
```

Related and separate: `vm/src/runtime/signals.rs` holds a **third** shutdown
mechanism — `SignalHandler` with its own `shutdown_hooks`, `add_shutdown_hook`,
`initiate_shutdown` and `run_shutdown_hooks` — whose only callers in the whole
tree are its own unit tests. It is Rust-`fn`-valued, unconnected to Java hooks,
and is the same write-only shape this record is about, one layer down. Whoever
closes the signal door should either wire it or delete it; leaving a second
green-looking shutdown API next to the real one is how a later lane concludes
signals are handled.

### 9.4 WITHDRAWN — N2 is already landed

Checked rather than assumed: `regression-suite/run.sh:106` already carries
`RShutdownHooks` in `CORE_CLASSES` (between `RJdkIntrinsics2` and
`RSimpleTimeZoneRaw`), so the vector is scheduled and the `STRICT_COVERAGE=1`
census is not at risk. Nothing to nominate; §3 N2 is closed. **The vector has
therefore been running red-or-vacuous against a VM that never ran a hook — it
is the first thing to re-read after this build.**

### 9.5 NOMINATION — two process-global statics that should be VM-scoped

`SHUTDOWN_HOOKS` was already process-global (W5-1 §357 calls it out as an
unpartitioned table) and `SHUTDOWN_IN_PROGRESS` now joins it. In the CLI there is
one VM per process so nothing latches wrongly, but an in-process second `Vm`
after a first one has shut down would find `addShutdownHook` throwing
`IllegalStateException` forever. Both belong on the VM-scoped table, together —
see `process-global-native-caches-must-be-vm-scoped`.

### 9.6 NOMINATION — `CRATONVM_SHUTDOWN_HOOK_TIMEOUT_MS` should be a declared flag

It is read with `std::env::var` behind a `OnceLock` because `nbflags()` lives in
`native-builtins/src/lib.rs`, which this lane does not own. It should move into
that struct with the others so `--help`, the flag-group expander and the
unknown-token guard all see it.
