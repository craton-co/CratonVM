# W7-92 — `Runtime.addShutdownHook` registers, and nothing ever starts the thread

Status: **OPEN**. Both modes. The registry is correct and complete; **execution
is absent on every one of the five exit paths.** Measured against HotSpot
25.0.3+9 on all five, plus `Runtime.halt`, `removeShutdownHook`, concurrent
hooks and duplicate registration. Lane C9, 2026-08-12.

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
