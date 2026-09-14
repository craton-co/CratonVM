# G11-1 — the shutdown-hook contract measured end to end, and the process cluster re-read

> **RECONCILED 2026-08-17 (lane G40) — two notes.**
>
> * **The provenance premise "no JDK source was read" was avoidable.**
>   `C:\craton\jdk25src` is indeed absent, and this record is right about that.
>   But the JDK's sources ship with the oracle itself, at
>   `$JAVA_HOME/lib/src.zip` (52,462,198 bytes) — the sources of the exact
>   HotSpot 25.0.3+9-LTS build used here, `java.lang.ApplicationShutdownHooks`
>   included. See `INDEX.md` §B.3.
> * **Its verification recipe needs one caveat.** The record asks for
>   `owns_slot=true`, `kind=bridge` and a **non-zero `invocations`** as proof.
>   That direction is sound and unchanged. The converse is not: `G33-1`
>   established that `invocations` is a **floor**, so a zero is not evidence of
>   absence. If the check comes back zero, re-run it under `--nojit` **and**
>   `CRATONVM_DISABLE_INTRINSICS=1`, where the counter is exact. See `INDEX.md`
>   §B.1.

**Status:** code landed in three files; **NOTHING HERE WAS RUN ON A CRATONVM
BINARY.** **Provenance:** every HotSpot cell is `MEASURED` on Temurin
25.0.3+9-LTS or `SOURCE-VERIFIED` by `javap` against the same image; every
CratonVM cell is `SOURCE-VERIFIED` or `PREDICTED` and is labelled as such.
Lane G11, 2026-08-17, branch `claude/jdk-only-mode-completion-1351c0`.

The lane was forbidden to build, to `cargo check`, or to run anything under
`regression-suite/` — the orchestrator was running the differential suite
concurrently. So this record obeys HANDOFF-20260814 §2 literally: **a
prediction is not a result, and none of the "after" values below is one.**

Probe: `scratchpad/g11/HookProbe.java` (single-file source mode, ASCII labels
only, 17 modes; run as
`"$JAVA_HOME/bin/java" scratchpad/g11/HookProbe.java <mode>`). Disassembly:
`javap -p -c java.lang.Shutdown | java.lang.ApplicationShutdownHooks |
java.lang.Runtime`. `C:\craton\jdk25src` is absent on this host, so the
bytecode IS the source here.

**Files changed (the only three this lane owns):**
`native-builtins/src/lang_system.rs`, `vm/src/runtime/signals.rs`,
`vm-cli/src/main.rs`.

---

## 0. The headline

W7-92's diagnosis is settled and its runner is present in the tree. This lane
did not re-litigate either. What it did was **measure the contract the runner
has to satisfy**, and that measurement found **five rows the landed runner
answers wrongly** — four of them fabricated successes, one of them the
delivery of the output itself.

| row | HotSpot 25.0.3+9 | CratonVM BEFORE | after (PREDICTED) |
|---|---|---|---|
| `addShutdownHook(null)` | `NullPointerException: Cannot invoke "java.lang.Thread.isAlive()" because "hook" is null` | **accepted, silently dropped** | throws, message transcribed |
| `removeShutdownHook(null)` | `NullPointerException`, `getMessage()` **null** | **returned `false`** | throws, message `null` |
| `addShutdownHook(runningThread)` | `IllegalArgumentException: Hook already running` | **accepted**, then `skipped` at shutdown | throws |
| an unflushed `System.out` write at exit | delivered | **PREDICTED lost** (fd 1 is a `Mutex<io::Stdout>`, i.e. a LINE writer, and `std::process::exit` runs no destructors) | flushed on all four exit paths |
| a second, dead shutdown API in `vm/src/runtime/signals.rs` | n/a | present, `pub`, unit-tested, **called by nothing** | deleted |

The first three are the **fabricated-success** class: a registration that
reports success and can never run is worse than one that refuses, because
nothing fails.

**The single most important sentence in this record:** the four counters
`run_shutdown_hooks` already prints are what turn any of this into evidence.
See §5 for exactly what to look for and what each outcome means.

---

## 1. The oracle's shutdown sequencing contract

All MEASURED unless marked. `rc` is the process exit status.

### 1.1 The call chain, SOURCE-VERIFIED

```
Runtime.exit(n)          -> Shutdown.exit(n)
System.exit(n)           -> Shutdown.exit(n)

Shutdown.exit(int):                        // synchronized (Shutdown.class)
     0: logRuntimeExit(status)
     9: beforeHalt()
    12: runHooks()                         // <- hooks happen HERE
    15: halt(status)                       // <- and the code is applied AFTER

Runtime.halt(n):
     0: Shutdown.beforeHalt()
     3: Shutdown.halt(n)   -> synchronized(haltLock) { halt0(n) }
                                           // NO runHooks. That is the point.

Shutdown.runHooks(): for slot in 0..10 { currentRunningHook = slot;
                                         hooks[slot].run() }   then VM.shutdown()
ApplicationShutdownHooks is slot 1, installed from its own <clinit>.
```

Two consequences fall straight out of that listing and are worth stating
because both are easy to get backwards:

* **the exit code survives the hooks** — `halt(status)` is reached *after*
  `runHooks()` returns, with the `status` `exit` was called with. A hook cannot
  change it except by calling `halt` itself. MEASURED: rc 3 (`exitmain`), rc 4
  (`exitother`, from a non-main thread), rc 7 (`throwing`, a hook threw), rc 9
  (`haltinhook`).
* **`Shutdown.exit` is `synchronized` on `Shutdown.class`**, which is why
  `System.exit` from inside a hook **deadlocks the JVM**. MEASURED: `exitinhook`
  printed `EXIT-HOOK-ENTERED`, `PEER-HOOK-RAN`, and then sat there —
  `timeout 12` fired, rc 124. `Runtime.halt` from inside a hook does not: rc 9,
  immediately.

### 1.2 `ApplicationShutdownHooks.add(Thread)`, SOURCE-VERIFIED, `static synchronized`

```
  0: getstatic hooks; ifnonnull 16   -> IllegalStateException "Shutdown in progress"
 16: aload_0; invokevirtual Thread.isAlive   -> NPE lands HERE for a null hook
 20: ifeq 33                         -> IllegalArgumentException "Hook already running"
 33: hooks.containsKey(hook)         -> IllegalArgumentException "Hook previously registered"
 53: hooks.put(hook, hook)
```

`remove(Thread)` is the same shape with the null check made **explicit**:
`hooks == null` at pc 0, then `new NullPointerException()` — the **no-arg**
constructor — at pc 20.

That ordering is the contract, not a preference. It says the shutdown check
outranks the null check on **both** methods, and it explains why the two NPEs
differ: `add`'s is HotSpot's helpful-NPE rendering of a real `invokevirtual`,
`remove`'s is a hand-thrown bare one.

### 1.3 The measured cells

Transcribed from `HookProbe`, not derived. `null` is printed distinctly from
`""` throughout; none of these messages is guessable from the method name.

| probe | cell |
|---|---|
| `DUP-ADD` | `threw=java.lang.IllegalArgumentException msg=Hook previously registered` |
| `RUNNING-ADD` | `threw=java.lang.IllegalArgumentException msg=Hook already running` |
| `TERMINATED-ADD` | `accepted` — a hook `Thread` that already ran to completion is legal |
| `NULL-ADD` | `threw=java.lang.NullPointerException msg=Cannot invoke "java.lang.Thread.isAlive()" because "hook" is null` |
| `NULL-REMOVE` | `threw=java.lang.NullPointerException msg=null` |
| `REMOVE-1` / `REMOVE-2` | `true` / `false` |
| `ADD-DURING` | `threw=java.lang.IllegalStateException msg=Shutdown in progress` |
| `REMOVE-DURING` | `threw=java.lang.IllegalStateException msg=Shutdown in progress` |

### 1.4 Concurrency and ordering

`ApplicationShutdownHooks.runHooks` takes the key set under the monitor, sets
`hooks = null` (which is what arms the ISE above), then **starts every hook**
and only afterwards **joins every hook**. So:

* **hooks are concurrent.** `crosswait`: `h-waiter` blocks on a
  `CountDownLatch` that `h-signal` counts down; output is `SIGNAL firing` then
  `WAITER released=true`. An inline runner answers `released=false` after
  burning a 10 s timeout, or deadlocks.
* **there is no order.** `order` registers `h-0 … h-4` and HotSpot printed
  `ORDER 2, 1, 4, 0, 3`. **A fix must never be judged on hook order.**
* **a throwing hook stops nothing.** `throwing`: all three hooks ran, the trace
  was printed by `h-boom`'s own thread, rc stayed 7.
* **an already-started hook is silently skipped.** `runHooks`' exception table
  catches `IllegalThreadStateException` around `Thread.start()` and jumps
  straight to the next hook — no diagnostic at all.

### 1.5 Path-by-path, MEASURED

| mode | HotSpot output (elided) | rc |
|---|---|---|
| `normal` (main returns) | `MAIN-END`, then `HOOK-OUT h-1/h-3/h-2`, `HOOK-FD1 h-1` | 0 |
| `exitmain` | `MAIN-END`, `HOOK-OUT h-1` | 3 |
| `exitother` (exit from another thread) | `HOOK-OUT h-1` — note `MAIN-END` never printed | 4 |
| `nondaemon` | `MAIN-END`, **`KEEPER-DONE`**, then `HOOK-OUT h-1` | 0 |
| `uncaught` | `MAIN-END`, **the stack trace**, then `HOOK-OUT h-1` | 1 |
| `halt` | no hook output at all | 5 |
| `haltinhook` | `PEER-HOOK-RAN`, `HALT-HOOK-ENTERED` | 9 |
| `exitinhook` | `EXIT-HOOK-ENTERED`, `PEER-HOOK-RAN`, then **hangs** | 124 (timeout) |

The `nondaemon` row confirms the ordering W7-92 §7.3 built the launcher around:
shutdown does not begin until the last non-daemon thread ends. The `uncaught`
row confirms §9.1's divergence is real and is still open here (§6).

### 1.6 Flushing — the row this lane was told to expect and did find

Two modes, both MEASURED:

* `noflush` — a hook does `System.out.print("HOOK-PARTIAL-NO-NEWLINE")` with no
  newline and no `flush()`, and main then calls `System.exit(0)`. The bytes
  **are delivered**.
* `haltnoflush` — an unterminated, unflushed `System.out.print`, then
  `Runtime.halt(6)`, which runs no hooks whatsoever. The bytes are **still
  delivered**, rc 6.

So HotSpot loses nothing at exit, on either path, and the flush obligation is
**independent of whether hooks run**. That distinction is what put the flush on
the `halt` path in this VM as well without putting hooks there — see §3.3.

---

## 2. What CratonVM did with those rows, before this lane

SOURCE-VERIFIED by reading `native-builtins/src/lang_system.rs` at HEAD.

* `Runtime.addShutdownHook` decoded its argument as
  `if let Some(Value::Object(Some(hook))) = args.get(1) { … }` and otherwise
  fell through to `Ok(None)`. **A null hook was accepted and dropped.** The
  Java caller saw a normal return.
* `Runtime.removeShutdownHook` matched the same pattern with `_ => false`. **A
  null hook was answered `false`** — a plausible, wrong "it was not
  registered".
* `shutdown_hook_add` had no `isAlive` arm at all, so a hook `Thread` that was
  already running was **accepted**, and `run_shutdown_hooks`' own
  `thread_already_started` check then counted it `skipped`. Registered, never
  runnable, reported successful.
* No exit path flushed the VM's console buffers. `FileDescriptorTable`'s fd-1
  and fd-2 entries are `Mutex<io::Stdout>` / `Mutex<io::Stderr>` — Rust's
  stdout is a **line writer** — and all three `exit`/`halt` natives end in
  `std::process::exit`, which runs no destructors.

**These are four separate defects and only the fourth is about hooks running.**
The first three are registration-time contract rows that were wrong whether or
not the runner works, and they would have stayed wrong behind a green
`RShutdownHooks`, because that vector does not exercise any of them (§7).

---

## 3. What this lane changed

### 3.1 `native-builtins/src/lang_system.rs` — the refusal ladder

`shutdown_hook_add` and `shutdown_hook_remove` now take
`Option<ObjectRef>` and own the **whole** ladder in §1.2's order:

```
ISE "Shutdown in progress"   ->  NPE  ->  IAE "Hook already running"  ->  IAE "Hook previously registered"
```

The null arm is deliberately **not** decided at the registration site. Deciding
it there would put the null check ahead of the shutdown check, which is the
opposite of what the bytecode does, and it is how the old silent-drop got
written in the first place.

The four messages are now named constants
(`HOOK_MSG_SHUTDOWN_IN_PROGRESS`, `HOOK_MSG_NULL_ADD`,
`HOOK_MSG_ALREADY_RUNNING`, `HOOK_MSG_PREVIOUSLY_REGISTERED`) so the unit tests
can pin them. HANDOFF-20260814 §5: *messages often cannot be derived, only
transcribed.*

A new `shutdown_hook_argument(args, which)` separates **a Java null** (which
must become an NPE) from **an argument vector that is not `[receiver, Thread]`**
(which must not — that would report a caller error for a VM fault). The second
case prints one unconditional stderr line and declines to decide. It is not
expected to fire; if it ever does, the line is the finding.

### 3.2 `lang_system.rs` — `flush_console_streams`

`ctx.fd_table().flush(1)` and `flush(2)`, as two independent statements so a
failure on the first cannot skip the second, with both results discarded — the
process is already committed to an exit code chosen elsewhere and a broken pipe
must not change it.

Called from four places:

| site | position |
|---|---|
| `run_shutdown_hooks` | after the join loop, **before** the `[cratonvm] shutdown hooks:` summary |
| `native_system_exit` | last statement before `std::process::exit(code)` |
| `native_runtime_exit` | same |
| `native_shutdown_halt0` | same — **flush only, still no hooks** (§1.6) |

The placement inside `run_shutdown_hooks` rather than only at the `exit` sites
is load-bearing twice over: the launcher's post-`main` path never reaches
either `exit` native, and the summary line is an `eprintln!` while a hook's
`System.out.println` went through the fd table — without the flush the two can
be delivered out of order in a `2>&1` capture, which is how every vector in
`regression-suite` is read.

### 3.3 `vm-cli/src/main.rs` — flush before the two `process::exit` paths

Both changes are **additive**: no new output, no reordering, **no change to any
exit code**.

* the `Err(e)` arm of the `main-vm` closure flushed stderr and then
  `std::process::exit(1)`. **Stdout was never flushed there**, so a program
  whose last `System.out` write had no trailing newline and which then died on
  an uncaught exception lost those bytes. Now flushes stdout after stderr.
* the `handler.join()` panic arm flushed neither. Now flushes both. A VM panic
  is the case where buffered application output is most worth having.

Nothing else in this file was touched. In particular the `run_shutdown_hooks`
call site, its position after `wait_for_non_daemon_threads`, and the
`match result` below it are byte-for-byte unchanged: this file is the process
entry point and the orchestrator is mid-suite.

### 3.4 `vm/src/runtime/signals.rs` — the second shutdown API is deleted

W7-92 §9.3 filed it as *"wire it or delete it; leaving a second green-looking
shutdown API next to the real one is how a later lane concludes signals are
handled."* Deleted, with the reasoning kept in the module header so the next
reader does not re-add it:

`SignalHandler::shutdown_hooks` / `add_shutdown_hook` / `remove_shutdown_hook` /
`run_shutdown_hooks` / `initiate_shutdown` / `next_hook_id`, the `ShutdownHook`
struct with its `priority` and `HookState`, `ShutdownResult`, and the twelve
unit tests that made all of it look healthy. **Grep confirms zero references
anywhere else in the tree** — the only callers were those tests.

It was not an unfinished version of the real thing; it was a different, wrong
thing, on three counts, each of which §1 measured:

* it ran hooks **inline** on the caller's thread (`crosswait` deadlocks);
* it ran them **in priority order** (the JDK has no priority and no order);
* its refusal string was `"Cannot add shutdown hook: shutdown in progress"`
  where HotSpot throws `IllegalStateException("Shutdown in progress")`.

What survives is the signal half — `registered_signals` plus `register_signal`
— now documented as a table and nothing more. **The signal door is still
shut**: no OS handler is installed from it and nothing in this VM converts a
signal into a call to `lang_system::run_shutdown_hooks`. See NOMINATION N1.

### 3.5 Which body wins the slot

Registration is last-write-wins with no unregister API, so this has to be
established rather than assumed.

`java/lang/Runtime.addShutdownHook` / `removeShutdownHook` are registered in
**exactly one place**, `lang_system::register_runtime_natives`, and that
function is called from two sites — `register_essential_natives_with_shims`
(`native-builtins/src/lib.rs:7327`, the real-JDK / `--jdk-only` arm) and
`register_synthetic_overrides` (`:23598`). Neither passes an explicit kind, so
both inherit the ambient category; the essential arm's is
`NativeKind::Bridge`, set at `lib.rs:7246`, **before** line 7327 with no
intervening `set_category`. `Bridge` is not gated by `CompatibilityMode::JdkOnly`
(`native-api/src/registry.rs`: `SyntheticStub` is the only refused kind), so
the bodies edited here **are** the bodies that run under `--jdk-only`.
SOURCE-VERIFIED. The only other mention of these two names outside this file is
`classloading/src/class_manager.rs:16406`, which **synthesises the method
declarations** for the synthetic carrier class — it declares, it does not
register a competing body.

**This is a source reading and it is not a registry dump.** HANDOFF-20260814 §4
is explicit that `--dump-native-registry` is what settles "which body runs", and
this lane could not run it. §5's checklist asks the orchestrator to.

### 3.6 Unit tests

New module `shutdown_hook_contract_tests` in `lang_system.rs`, seven tests:

* the four messages are byte-exact against the transcription;
* the four messages are **pure ASCII** — HANDOFF-20260814 §7's em-dash
  differential is the reason this is checked rather than eyeballed;
* `shutdown_hook_argument` distinguishes a Java null (`Some(None)`) from a
  short vector (`None`) from a reference (`Some(Some(_))`) — five cases;
* `addShutdownHook(null)` throws NPE carrying the helpful message and leaves
  the registry empty;
* `removeShutdownHook(null)` throws NPE that does **not** carry `add`'s
  message;
* shutdown-in-progress **outranks** the null check on both methods;
* the accept / duplicate-IAE / `remove` true-then-false sequence, with the
  registry length asserted after the refusal so a refused registration cannot
  leave an entry.

Every assertion matches on the `RuntimeError` **variant and message string**,
never on `format!("{failed:?}")`. That is not style: `Debug` escapes the
embedded quotes, so `HOOK_MSG_NULL_ADD` — which contains
`"java.lang.Thread.isAlive()"` — never appears verbatim in a `Debug` rendering,
and a `.contains()` check against it passes or fails for reasons that have
nothing to do with the VM. This module's first draft had exactly that bug.
Matching on the variant is also the only way to assert `getMessage() == null`
rather than `== ""`.

`SHUTDOWN_HOOKS` and `SHUTDOWN_IN_PROGRESS` are process-global, so every test
that touches either takes a module-local mutex and restores both. **The
`Hook already running` arm is NOT unit-tested** — `test_utils`' mock answers
`thread_is_alive == false` unconditionally, so that arm is reachable only in
the VM. It is pinned by the constant test and by §5's checklist, and saying so
here is the point.

---

## 4. What this lane did NOT do

Stated plainly, because every one of these is a place a reader could otherwise
assume more was settled than was.

* **Nothing was built, checked, or run on CratonVM.** No `cargo build`, no
  `cargo check`, no `cargo test`, no `--dump-native-registry`, no
  `regression-suite` vector, no `cratonvm.exe` of any kind. Every CratonVM
  "after" in this record is `PREDICTED`.
* **It did not verify the W7-92 runner works.** The runner was read, not
  exercised. The defects fixed here are *upstream* of it (registration) and
  *downstream* of it (delivery); whether the middle — `invoke_virtual(hook,
  "start")` on a real thread, the bounded join, the blocked-region protocol —
  actually runs a hook is exactly as unverified as it was before.
* **It did not touch the `uncaught` ordering divergence** (W7-92 §9.1: HotSpot
  prints the trace then the hooks; CratonVM prints hooks then trace). MEASURED
  on the oracle here and confirmed real, but fixing it means restructuring how
  `vm-cli` renders a fatal exception, which changes output every harness in
  this tree reads — with a suite running. Left open, deliberately.
* **It did not fix `System.exit`-inside-a-hook** (W7-92 §9.2). HotSpot hangs
  (MEASURED, rc 124 under a 12 s bound); CratonVM terminates. A divergence in
  CratonVM's favour, restated rather than closed.
* **It changed nothing in `native-io/src/process.rs` or
  `native-builtins/src/phases_late.rs`.** The entire process cluster of §8 is
  out of lane and is filed as nominations only.
* **It did not edit `RShutdownHooks.java`, `run.sh`, `INDEX.md` or
  `README.md`.** §7 gives the missing Java as a code block instead.
* **It did not run the differential.** No cross-VM diff was produced by this
  lane at all.

---

## 5. What the orchestrator must check, and what each outcome means

This is the section HANDOFF-20260814 §5 exists for — *"a green build proves you
broke nothing, not that you did something"* — and for a shutdown hook the
failure is especially quiet, because the process exits either way and a missing
hook looks like a clean run.

### 5.1 At build time

1. `cargo check --workspace --tests` must be clean. **The riskiest change for
   compilation is §3.4**: `vm/src/runtime/signals.rs` lost seven `pub` items.
   Grep says nothing outside that file referenced any of them; a build is what
   proves it.
2. `native-builtins` depends on `native-io` (HANDOFF §7) — if `native-io` fails
   to compile, **nothing downstream is checked at all**, and "everything else
   compiled" is not a statement that can be made from that build.
3. `cargo test -p cratonvm-native-builtins shutdown_hook_contract_tests` — seven
   tests, all of which should pass on the changed tree and at least three of
   which fail on the tree before it.

### 5.2 The observable that distinguishes "the fix worked" from "the fix did nothing"

Run `RShutdownHooks` and read **two** things, not one:

```
[cratonvm] shutdown hooks: ran=N threw=N skipped=N unjoined=N trigger=main-returned
CK RShutdownHooks hookFd1 ran=true ownThread=true err=ok out=ok
```

| what you see | what it means |
|---|---|
| `ran=1` **and** the three `CK … hook*` lines | the runner works **and** delivery works. Both halves. |
| `ran=1` and **no** `hook*` lines | the runner works, **delivery does not**. §3.2's flush is in the wrong place or fd 1 is not the path `System.out` takes. This is the state §3.2 exists to prevent and it is now distinguishable from the next row. |
| `ran=0` while a hook was registered | the runner never saw the hook. Registration is the suspect, not the runner: dump the registry and check who owns `java/lang/Runtime.addShutdownHook`. |
| no `[cratonvm] shutdown hooks:` line at all | `run_shutdown_hooks` was not reached on this path. That line is unconditional. |
| `hookFd1 … out=LOST` | the hook ran and `System.out` is dead — a third state, and the reason `RShutdownHooks` writes on three channels. |

`extract()` in `regression-suite/harness-guard.sh` keeps only `^(PASS|CK) `, so
the `[cratonvm]` line is filtered out of the cross-VM diff and cannot itself
become a difference. **Read it from the raw capture, not the filtered one.**

### 5.3 The registry question this lane could not answer

`--dump-native-registry out.json` and check the two rows

```
java/lang/Runtime  addShutdownHook     (Ljava/lang/Thread;)V
java/lang/Runtime  removeShutdownHook  (Ljava/lang/Thread;)Z
```

for `owns_slot=true`, `kind=bridge`, and a **non-zero `invocations`** after a
run of `RShutdownHooks`. §3.5 predicts all three from source. `owns_slot=true`
plus non-zero `invocations` is the proof; reading cannot settle it.

Flags must precede the main class or they are ignored silently, with exit 0 and
no file.

---

## 6. Records that can close, and on what evidence

| record | disposition | evidence |
|---|---|---|
| `W7-92` §8 (the four contract questions) | **CONFIRMED and EXTENDED.** Its (a)–(e) all reproduce on 25.0.3+9. Three rows it did not have are added: null-add NPE, null-remove NPE-with-no-message, and `Hook already running`. | §1.3, MEASURED |
| `W7-92` §9.3 (the third shutdown mechanism in `signals.rs`) | **CLOSED — deleted.** | §3.4, SOURCE-VERIFIED |
| `W7-92` §9.1 (uncaught ordering) | **STILL OPEN**, and now MEASURED on the oracle rather than recalled: trace, then hooks, rc 1. | §1.5 |
| `W7-92` §9.2 (`System.exit` inside a hook) | **STILL STATED.** HotSpot's hang is now MEASURED (rc 124 at a 12 s bound), where §8(d) had it bounded by the probe runner. | §1.1 |
| `W7-92` headline (`FIXED-UNVERIFIED`) | **UNCHANGED.** Do not move it. This lane added contract rows and delivery; it verified nothing on a binary. | §4 |
| `INDEX` line for shutdown hooks | **UNCHANGED, and still correct as written.** "MEASURED never to run … the fix is still unverified" describes a binary that predates commit `c59efd3eb` (2026-08-13 00:11), which is where the runner landed. Nothing here upgrades it. INDEX is shared and was not edited. | §4 |
| `P4A-SPRING-20260812` §4 | **CONSISTENT.** Its `HOOK-RAN A/B` absence is the same measurement, on the same pre-runner binary. Its Spring row (`registerShutdownHook()` never destroys beans) is the same defect wearing another hat and closes with the same evidence. | — |
| `W6-10` findings 1, 2 and 4 | **CLOSED IN SOURCE.** See §8.3. | SOURCE-VERIFIED |
| `W7-10` §7.3 | **APPLIED** (already noted in that record). §7.1 and §7.2 remain open; §8.2 below adds a new one. | SOURCE-VERIFIED |
| `W7-46` §8.2 (four double-registered `ProcessBuilder` triples) | **THREE CLOSED, ONE REMAINS.** | §8.4 |

`W7-97` was read for the reason the brief gives — shutdown wiring often lives
near VM init phase ordering — and it does not bear on any of this. Its
correction is that `initPhase2`'s skip is right for a `java.nio` reason rather
than a module-graph one; the module system is initialised in `vm-cli` in the
`initPhase2` slot, and no shutdown machinery passes through there. Recorded so
the next reader does not have to check again.

---

## 7. Does `RShutdownHooks` cover the contract? — no, it covers about half

`regression-suite/src/RShutdownHooks.java` was read (read-only). It is a good
vector and its three-channel design is exactly right: it is the reason
"the hook ran and its output was lost" was ruled out, and that ruling is what
made the whole diagnosis safe. What it covers:

* `removeShutdownHook` true then false;
* duplicate registration → `IllegalArgumentException` (as a boolean, not the
  message);
* a registered hook is not alive yet;
* a **removed** hook does not run — the negative arm, without which a VM that
  ran every `Thread` it had ever seen would pass;
* the hook runs, on its own thread, with output on three channels;
* the `main`-returns path only.

**What it does not cover, and each of these is a row measured in §1:**

1. `addShutdownHook(null)` and `removeShutdownHook(null)` — both of them
   fabricated successes on this VM until today, both invisible to this vector.
2. the **message** of the duplicate IAE (`Hook previously registered`).
3. `addShutdownHook` of an already-running `Thread` → `Hook already running`.
4. `addShutdownHook` of an already-**terminated** `Thread` → accepted (the
   record's `TERMINATED-ADD`; the file's comment mentions it and then
   deliberately does not assert it).
5. the exit-code-survives-hooks row, and every path other than `main`-returns.
   The file explains why (a vector that terminates the process cannot print its
   own PASS line) and points at the `ShutdownProbe` matrix instead — which is
   still owed.

Items 1–4 are all registration-time, all synchronous, and all assertable from a
vector whose `main` returns normally. They belong here. **The suggested
addition, for whoever owns the file — this lane did not edit it:**

```java
        // --- G11-1: the registration contract, MEASURED on Temurin 25.0.3+9.
        // All of these are synchronous and none of them terminates the JVM, so
        // they belong in the main-returns vector rather than the ShutdownProbe
        // matrix.

        // addShutdownHook(null) -> NPE. HotSpot has no explicit null check:
        // ApplicationShutdownHooks.add pc 17 does `hook.isAlive()`, so the
        // message is the helpful-NPE rendering of THAT call site.
        String addNullMsg = "ABSENT";
        try {
            rt.addShutdownHook(null);
        } catch (NullPointerException e) {
            addNullMsg = String.valueOf(e.getMessage());
        }
        check(addNullMsg.contains("isAlive"),
                "addShutdownHook(null) must throw NPE naming isAlive(), got " + addNullMsg);

        // removeShutdownHook(null) -> NPE with a NULL message (pc 20 is a
        // no-arg `new NullPointerException()`), which is a different cell from
        // the empty string and must print as such.
        String rmNullMsg = "ABSENT";
        boolean rmNullThrew = false;
        try {
            rt.removeShutdownHook(null);
        } catch (NullPointerException e) {
            rmNullThrew = true;
            rmNullMsg = String.valueOf(e.getMessage());
        }
        check(rmNullThrew, "removeShutdownHook(null) must throw NullPointerException");
        check("null".equals(rmNullMsg),
                "removeShutdownHook(null) NPE must carry no message, got " + rmNullMsg);

        // The duplicate IAE's MESSAGE, not just its type.
        String dupMsg = "ABSENT";
        try {
            rt.addShutdownHook(live);
        } catch (IllegalArgumentException e) {
            dupMsg = String.valueOf(e.getMessage());
        }
        check("Hook previously registered".equals(dupMsg),
                "duplicate add message must be 'Hook previously registered', got " + dupMsg);

        // An ALREADY RUNNING Thread is refused with a DIFFERENT message
        // (pc 27), because isAlive() is tested before containsKey().
        java.util.concurrent.CountDownLatch hold =
                new java.util.concurrent.CountDownLatch(1);
        Thread running = new Thread(() -> {
            try { hold.await(); } catch (InterruptedException e) { }
        });
        running.setName("cratonvm-hook-running");
        running.start();
        String runningMsg = "ABSENT";
        try {
            rt.addShutdownHook(running);
        } catch (IllegalArgumentException e) {
            runningMsg = String.valueOf(e.getMessage());
        }
        hold.countDown();
        running.join();
        check("Hook already running".equals(runningMsg),
                "adding a running hook must say 'Hook already running', got " + runningMsg);

        // A hook Thread that already ran to completion is ACCEPTED, and is not
        // re-run. Registered LAST so its acceptance cannot disturb the rows
        // above; it contributes no output, which is itself the assertion.
        Thread dead = new Thread(() ->
                System.out.println("CK RShutdownHooks DEAD-HOOK-BODY-RAN"));
        dead.setName("cratonvm-hook-dead");
        dead.start();
        dead.join();
        rt.addShutdownHook(dead);   // must not throw

        System.out.println("CK RShutdownHooks reg addNullNPE=true rmNullMsg=" + rmNullMsg
                + " dup=" + dupMsg + " running=" + runningMsg + " deadAccepted=true");
```

Note the two traps that shape it. `DEAD-HOOK-BODY-RAN` is printed **once**, by
the explicit `start()`, and must not appear a second time — one line, and the
count is the assertion; a VM that re-runs a terminated hook prints two. And
every label stays ASCII (HANDOFF §7). Adding these five rows takes the vector's
count from 4 to 10; `harness_check_count` parses `PASS <Class> (N checks)`, so
the existing `PASS` line updates itself.

---

## 8. The process cluster

**Everything in this section is out of lane.** The lane owns three files and
none of them is `native-io/src/process.rs` or
`native-builtins/src/phases_late.rs`. Nothing here was changed; it is a
re-read, and it is `SOURCE-VERIFIED` throughout — no CratonVM process natives
were executed.

Per the brief, **`W7-10` §6's stub-count deltas are declared dead and are not
quoted here.**

### 8.1 Fabricated bodies still live on the `ProcessHandle` surface

A `ProcessHandle.Info` that answers plausible values it never obtained is worse
than one that refuses, because nothing fails. The good news first: the six
`$Info` accessors registered by `register_p60_process_handle`
(`phases_late.rs:2890-2956`) are **honest** — `commandLine`, `arguments`,
`user`, `startInstant`, `totalCpuDuration` all return
`Optional.empty`, and `command` returns a real value **only** for this VM's own
pid and refuses for every other. That is W7-10's fix and it is present.

What is still fabricated:

| site | body | why it is a fabrication |
|---|---|---|
| `phases_late.rs:2356-2363` `p60_pid_is_alive`, non-unix arm | returns `true` for any pid | `ProcessHandle.isAlive()` answers "alive" about a process it never looked at. Self-documented. |
| `phases_late.rs:2503-2528` `p60_unmeasurable_process_tree` | empty stream for `children`/`descendants` | its own doc says *"It IS a fabrication"* — an empty tree is an assertion that there are no children. |
| `phases_late.rs:2179-2187` `p60_parent_pid`, Windows arm | returns `std::process::id()` | the process is reported as **its own parent**. |
| `phases_late.rs:2786-2799` `onExit` fallback | `p58_new_cf(ctx, this, done=true)` | a **completed** future asserts the process has already exited. |
| `phases_late.rs:2826-2834` `compareTo` | `unwrap_or(0)` on both operands | two unreadable handles compare **equal**. |
| `phases_late.rs:2863` `info()` | `p60_handle_pid(...).unwrap_or(0)` | an `Info` stamped pid 0. |
| **`native-io/src/process.rs:3814-3818`** `build_process_handle` | `new_object_initialized("java/lang/ProcessHandleImpl", "(JJ)V", [pid, 0])` | **the live one.** See §8.2. |

`supportsNormalTermination` answering a per-platform constant
(`cfg!(unix)`) is **not** a fabrication — the real
`ProcessHandleImpl.supportsNormalTermination()` is also a per-platform
constant, and `p60_handle_destroy`'s non-unix arm agrees with it.

### 8.2 NEW — `startTime = 0` is back, at a second site

`native-io/src/process.rs:3814-3818` stamps `startTime = 0` into every
`ProcessHandleImpl` it mints. This is the exact hardcoded `STARTTIME_ANY` that
W5-2 removed from `p60_process_handle_current`, at a different site.

`ProcessHandleImpl.info()` is `Info.info(pid, this.startTime)`, and the JDK's
own `startTime != info.startTime` guard then wipes `command`, `arguments`,
`startTime` and `totalTime`. So **`Process.toHandle().info()` and every handle
that comes out of `Process.descendants()` return an entirely empty `Info`** —
silently, because an empty `Optional` is legal. `equals`/`isAlive` are
unaffected (they wildcard 0), which is exactly why it survives unnoticed.

This is the same fabricated-success shape as the shutdown hooks: an API that
answers, and whose answer means nothing.

### 8.3 `W6-10` — treat it as a performance item, and most of it is already true

Its residual is un-appliable as written because its target record left the
directory. What is still true, SOURCE-VERIFIED:

* **finding 1 (one snapshot per tree node) — FIXED, and now partly moot.**
  `collect_descendant_pids` (`process.rs:3982-4019`) takes exactly one
  snapshot, indexes it into a `HashMap<parent, Vec<child>>`, and walks — one
  snapshot, `O(N + D)`, zero `OpenProcess`. But `native_process_descendants`
  (`:4099-4112`) now routes a real `java.lang.ProcessImpl` receiver through
  `toHandle().descendants()`, so under `--jdk-only` that optimised walk is not
  even reached; `ProcessHandleImpl.getProcessPids0` is. **The optimisation is
  now Compatible-mode only.** The non-Windows arm still reads
  `/proc/<pid>/task/*/children` per node, which is a read of the node being
  expanded rather than a machine-wide scan — there is nothing to hoist.
* **finding 2 (one `OpenProcess` too many) — FIXED.** `info0` takes both start
  time and CPU from one probe (`process.rs:5006`), and `isAlive0` uses a single
  `win_liveness_and_start_time`.
* **finding 4 (a failed scan reporting an empty machine) — FIXED.**
  `os_snapshot_processes` returns `Result` and the three callers convert to a
  real `java.lang.RuntimeException`.
* **still true, and the record's table does not say so:** `info()` on Windows
  now costs **three** opens, not the two W6-10 priced — `GetProcessTimes`,
  `QueryFullProcessImageNameW`, and the `OpenProcess`+`OpenProcessToken` pair
  the `user` probe added. They are genuinely different APIs but could share one
  handle. That is the only free `OpenProcess` left on this surface, and it is a
  performance item, not a correctness one.
* **a re-entry through the argument path**, which is finding 4's shape at a
  different door: `getProcessPids0` (`:4858`) answers **count 0 without
  scanning** when its first argument is not a `Long`. Same at `parent0`
  (`:4825`, answers `-1`) and `info0` (`:4984`), which returns leaving an
  entirely default `Info`.

### 8.4 `W7-46` — three of the four double registrations are gone

`native-builtins/src/lib.rs:39685-39699` is now the replacement comment W7-46
§8.2 prescribed, so the three `SyntheticStub → Intrinsic` kind rewrites are
closed. **`ProcessBuilder.start` is still registered twice**
(`phases_late.rs:1487` ambient `SyntheticStub`; `process.rs:5558` explicit
`SyntheticStub`) — same callback, same kind, and `register_io_natives` runs
last on every arm, so `process.rs:5558` owns the slot and the duplicate is
benign by construction.

Three the record does not list, all on `java/lang/ProcessHandle`, all
`SyntheticStub`, all won by `register_p60_process_handle`:
`current` (`1685` vs `2668`), `pid` (`1692` vs `2674`), `isAlive` (`1700` vs
`2678`). The `pid` pair is two **distinct closures with identical bodies** —
the twin shape this tree has a written record of drifting.

`scripts/baselines/jdk-only-kind-map-25-linux.tsv` still records these
duplicates as separate rows and still calls the whole `ProcessHandle`/`$Info`
surface `bridge`, which no longer matches source.

---

## 9. NOMINATIONS

Everything below is outside this lane's three files. Each carries file, line
and change.

**N1 — `native-builtins/src/lib.rs`, the `jdk/internal/misc/Signal.handle0`
comment (around `:14821-14830`).** W7-92 §9.3 already wrote the replacement
text and gated it on `RShutdownHooks` being green. That gate has **not** been
satisfied by this lane and the nomination is unchanged: apply §9.3's *new*
paragraph only after the orchestrator has run the vector against a build. Half
the sentence — "the shutdown hook is accepted and never runs" — is now wrong in
source; the other half — the signal door — is still exactly right, and §3.4 of
this record makes that explicit on the `signals.rs` side.

**N2 — `native-io/src/process.rs:3814-3818`.** Replace
`Value::Long(0)` with the process's real start time. `p60_real_handle_for`
(`phases_late.rs:2463-2468`) already routes through
`ProcessHandleImpl.getInternal(pid)` for precisely this reason; the same route,
or a `current_process_start_time`-style read via `isAlive0`, closes it. This is
§8.2 and it is the highest-value row in the cluster: it silently empties every
`Info` reachable from `Process.toHandle()` or `Process.descendants()`.

**N3 — `native-builtins/src/phases_late.rs:2356-2363`, `p60_pid_is_alive`
non-unix arm.** Replace the constant `true` with the Windows liveness probe
`native-io/src/process.rs` already has (`win_liveness_and_start_time`), or
refuse. A constant `true` is a fabricated answer to a question about another
process.

**N4 — `native-builtins/src/phases_late.rs:2179-2187`, `p60_parent_pid`
Windows arm.** Returning `std::process::id()` makes the process its own parent.
`os_parent_pid` (`native-io/src/process.rs:4709-4718`) reads
`th32ParentProcessID` out of the snapshot with zero `OpenProcess` and is the
right source.

**N5 — `native-builtins/src/phases_late.rs:2826-2834`, `ProcessHandle.compareTo`.**
`unwrap_or(0)` on both operands makes two unreadable handles compare equal,
which breaks the `Comparable` contract silently. Refuse instead.

**N6 — `native-builtins/src/phases_late.rs:2786-2799`, the `onExit` fallback.**
A pre-completed `CompletableFuture` asserts the process has exited. An
uncompleted future, or a refusal, is honest; this is not.

**N7 — `native-io/src/process.rs:1258-1284`, the `Redirect` decoder.** Three
manufactured defaults on one path: `_ => StdioRedirect::Pipe`,
`enum_ordinal(...).unwrap_or(0)`, and `.unwrap_or(StdioRedirect::Null)` for an
unreadable `File`. A redirect the caller asked for is silently replaced by a
different one and the child's output goes somewhere else with no exception.

**N8 — `native-io/src/process.rs:3770-3773`, `Process.toHandle`.** A receiver
whose pid slot is not a `Long` yields a `ProcessHandle` for **pid -1** rather
than throwing. Same shape at `:4858` / `:4825` / `:4984` (§8.3's last bullet).

**N9 — `native-io/src/process.rs:3606`.**
`let _ = ctx.invoke_virtual(thread, "setDaemon", "(Z)V", &[Value::Int(1)]);` —
a failed `setDaemon` leaves the reaper thread **non-daemon**, which keeps the
VM alive past `main`. That interacts directly with §1.5's `nondaemon` row: the
launcher waits for non-daemon threads *before* running hooks, so this failure
mode presents as "shutdown hooks never ran" while actually being "the VM never
reached shutdown".

**N10 — `scripts/baselines/jdk-only-kind-map-25-linux.tsv`.** Stale on the
whole `ProcessHandle`/`$Info` surface: records it as `bridge` where source says
`SyntheticStub`, carries the resolved duplicate rows, and predates
`$Info.commandLine`. Regenerate.

**N11 — `native-builtins/src/lang_system.rs` is where
`CRATONVM_SHUTDOWN_HOOK_TIMEOUT_MS` is read with a bare `std::env::var`,
because `nbflags()` lives in `lib.rs`.** W7-92 §7 already noted it; restating
it as a nomination so it is on one list with the rest. Also W7-92 §9.5's
`SHUTDOWN_HOOKS` / `SHUTDOWN_IN_PROGRESS` process-globals — this lane's unit
tests have to take a mutex and restore both, which is the cost of that being
unpartitioned, made visible.

---

## 10. Things that are true and easy to disbelieve

* **HotSpot deadlocks on `System.exit` inside a shutdown hook.** Not a slow
  path, not a long join — `Shutdown.exit` is `synchronized (Shutdown.class)`
  and the hook thread waits on a monitor the exiting thread will never release.
  rc 124 under `timeout 12`. CratonVM does *not* reproduce this and that is the
  better behaviour, but it is a divergence and it must be written down rather
  than discovered.
* **Five hooks registered `h-0 … h-4` came back `2, 1, 4, 0, 3`.** Any test
  that pins hook order is testing the scheduler.
* **`Runtime.halt` loses no buffered output.** It runs no hooks, it takes no
  monitor an application can hold, and it still delivered an unterminated,
  unflushed `System.out.print`. The flush obligation and the hook obligation
  are independent, which is why one of them is on the `halt` path here and the
  other is not.
* **`addShutdownHook(null)`'s NPE message names `isAlive()`, not
  `addShutdownHook`.** It is HotSpot's rendering of the bytecode at
  `ApplicationShutdownHooks.add` pc 17, and the quoted parameter name is
  `add`'s, not `Runtime`'s. Nothing about it is derivable from the API the
  caller used.
