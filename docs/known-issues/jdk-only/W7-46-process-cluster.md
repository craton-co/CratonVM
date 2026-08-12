# The process cluster re-run: two inherited records were discharged, one was live, and the sweep they licensed found three more

**Status:** SOURCE-ONLY. **No CratonVM binary was built or run.** Lane W7-46,
2026-08-12, worktree `CratonVM-procnat-20260812`, branch
`fix/processimpl-natives-and-checks-20260812`.

Every Rust claim is either a `javap` census against the JDK 25.0.3 Windows image
on this host, or provable by reading the code; the ones that are neither are
named as unproven at the point they are made, and no speedup is claimed anywhere.

**One thing WAS executed, and it earned its place:** the modified
`RJdkProcess.java` was compiled and run against real HotSpot 25 — the oracle,
which needs no CratonVM build. That is what seeded `EXPECTED_CHECKS` from a
measurement instead of a hand-count, and it is also what caught this lane's own
first attempt at the `descendants()` check being unsound (§3). A fixture change
is testable on the oracle alone; not doing so is a habit worth losing.

**Files changed:** `native-io/src/process.rs`,
`regression-suite/src/RJdkProcess.java`.

## What this lane was handed, and what was actually left of it

Three inherited records, re-verified row by row rather than triaged by their own
ranking:

| record | claimed | measured on this branch |
| --- | --- | --- |
| W3-6-processimpl-missing-natives.md | "FIX LANDED, UNVALIDATED" + an unapplied out-of-file patch | **discharged.** All ten Windows `ProcessImpl` natives are registered, under the corrected `create` descriptor, and the out-of-file patch landed separately as W7-10 |
| W5-2-two-silently-skipped-process-checks.md | FIXED, two residuals closed 08-11 | **discharged as to the defect**, but the *mechanism* that hid it was never removed — see below |
| W6-10-process-enumeration-syscall-cost.md | FIXED, findings 1/2/4 | **findings 1, 2 and 4 confirmed present.** Finding 2's shape survives on the neighbouring native, unnoticed |

So two of three were already true when this lane opened. What was not true is
the thing all three share: the *reason* W5-2's defect went four waves undetected
is still in the tree, and re-reading the surface with that shape in hand found
three unrecorded instances of it.

## 1. W5-2 — the two checks run; the mechanism that hid them did not change

The fix is in place and verifiable by reading: `start_time_or_any(pid)` is the
single source that `isAlive0`, `ProcessHandleImpl$Info.info0` and
`current_process_start_time` all answer from, so
`ProcessHandleImpl$Info.info(pid, startTime)`'s bare `!=` cannot wipe the record.
`info0` writes `startTime` unconditionally on every platform. Both residuals
(`Info.user`, `Info.totalTime` on Linux) are sourced from the OS.

**But the vector was unchanged.** `RJdkProcess` still printed `checks=N` and
asserted nothing about `N`. A check suppressed by a legally-false guard still
produced exit 0, `PASS`, no exception, and a two-digit difference that only a
human diffing the transcript against HotSpot's could see. The record's own
closing paragraph says so — *"a `check(...)` inside an `if` is only as good as
the count that surrounds it"* — and then leaves the count as a printed
observation.

Fixed in `regression-suite/src/RJdkProcess.java`, and deliberately NOT by
asserting the two `Optional`s unconditionally: they are legally empty on a
restricted platform and `ProcessHandle.Info`'s javadoc provides for it.

* `skip(why)` — records the reason and **increments `checks`**, so the count is
  invariant across conforming JVMs.
* `EXPECTED_CHECKS`, asserted in `main` before anything is printed. A moved
  count is now an `AssertionError` naming the expected total, the actual, and
  the skip list.
* `skipped=[…]` is printed on the `CK` line, which `regression-suite/run.sh`
  diffs against HotSpot.

Two severities, neither silent: a legal-but-degraded answer is a **textual**
diff against the oracle; a check that stopped running at all is a **hard
failure**. The count that used to be the only evidence is now the assertion.

`EXPECTED_CHECKS` is 55 — the 53 HotSpot ran, plus the two new checks this lane
added. **Measured on the oracle, three times, not counted by hand** (see §3). The `check(exit == null, "unreachable")` inside the `onExit()` try-block
is deliberately not counted: HotSpot never reaches it. A VM that fails to throw
runs it, the count becomes 56, and the constant catches that too.

## 2. The population sweep — how many others of this shape are there?

The brief asked for the real population of "a guard whose false branch returns
success" on the process surface, not an anecdote. Every `#[cfg]`, every early
return, and every `if` in `native-io/src/process.rs`, plus the process
registrations in `native-builtins/src/phases_late.rs` and
`native-builtins/src/lib.rs`, was read for it. **Fourteen sites, of which three
were live defects.** The other eleven are recorded here so the next reader does
not re-derive them.

### Live — fixed by this lane

| # | site | what the false branch answered | why it is a defect |
| --- | --- | --- | --- |
| 1 | `native_process_descendants` | an empty stream | **§3 below.** A concrete `java.lang.Process` method the real `ProcessImpl` does not override, with no `is_vm_process` guard, reading a pid slot off a JDK-layout receiver |
| 2 | `native_process_impl_create`, unreadable `cmdstr` | handle `0` | the JDK caller has no null-handle check; `start()` returns a live `Process` naming no process |
| 3 | `native_process_impl_create`, empty tokenisation | handle `0` | same, and this is the **reachable** arm: `new ProcessBuilder("").start()` |

### Read and cleared, with the reason

| site | false branch | verdict |
| --- | --- | --- |
| `native_process_impl_close_handle` | unconditional `true` | **not a fabrication.** The value is a table key; the OS handle is owned by the `std::process::Child` whose `Drop` closes it. Documented at the site |
| `native_process_impl_wait_for_interruptibly` / `_wait_for_timeout` | `Ok(None)` on an unknown handle | **unreachable by construction.** `spawn_child` installs every handle it mints in `exit_cache`, which is what `known_handle` reads, and the only producer of the `long` these take is `create`. Defensive, not silent |
| `native_process_impl_terminate` | no-op on an unknown handle | same, and the same producer argument |
| `native_process_impl_get_exit_code` / `_is_process_alive` | `-1` / `false` on an unknown handle | **correct, and load-bearing.** `try_exit_handle` answers `None` for both "running" and "no such child"; `known_handle` is what separates them. Answering `STILL_ACTIVE` here would make `waitFor()` throw from a method specified never to throw |
| `native_proc_handle_parent0`, `_get_process_pids0`, `destroy0` — argument-shape arms | `-1` / `0` | **not reachable from bytecode.** The descriptors are fixed by the image and every argument arrives typed. Left as-is; tightening them would be a guard against a caller that cannot exist |
| `foreign_pid_is_alive`, `not(any(linux, windows))` arm | optimistic `true` | **deliberate and documented.** A confident `false` would claim a running process had exited |
| `os_parent_pid` (Linux), unreadable `/proc/<pid>/status` | `Ok(-1)` | **correct, and the asymmetry is the point.** One unreadable status means that process is gone; a failed `opendir("/proc")` means the scan did not run and is an `Err`. W6-10 finding 4 established this split |
| `direct_child_pids`, unreadable `task/` | `Ok(vec![])` | same: a descendant that dies mid-walk must not abort the walk |
| `start_time_matches`, unknown start time | counts as a MATCH | documented at the site — "unknown is not disagreement" |
| `p60_unmeasurable_process_tree` | empty stream | **synthetic-JDK only, and refused under strict**, where the `SyntheticStub` tag drops the registration that reaches it |
| `p60_empty_optional` on the five `$Info` accessors | `Optional.empty()` | **the specified answer**, not a concession — the JDK's own accessors derive exactly this from an unwritten field |

### Two-gate `#[cfg]` with a default-off inner gate: **none**

`native-io/src/process.rs` contains no `feature = …` gate at all — every `#[cfg]`
in it is `windows` / `target_os = "linux"` / `not(any(…))`, and the three arms
partition the target space with no default-off hole. No `TODO`, `FIXME`,
`todo!()` or `unimplemented!()` anywhere in the file. The documented hole
species is absent from this surface; that is a measurement, not an assumption.

### One shape recorded, not fixed — out of lane

`java/lang/ProcessBuilder.<init>(Ljava/util/List;)V`,
`<init>([Ljava/lang/String;)V`, `command()Ljava/util/List;` and
`start()Ljava/lang/Process;` are registered by **two** registrars:
`phases_late::register_phase57_process`, which carefully restates
`SyntheticStub` for the whole ProcessBuilder cluster so strict mode refuses it,
and an untagged block in `register_enterprise_natives`
(`native-builtins/src/lib.rs`) which inherits the ambient category.

It does not bite on the measured paths, and the reason is worth stating rather
than assuming: `register_enterprise_natives` is reached only from
`register_synthetic_overrides`, which runs only when `use_synthetic_jdk` is true
at runtime. In real-JDK and `--jdk-only` boots it never executes, so it can
neither win the slot nor smuggle an accepted kind past the re-tag. In
synthetic-JDK mode it runs **after** `register_phase57_process` and wins all
four. That is the arm this lane cannot build or measure. Recorded for whichever
lane owns the synthetic-jdk gate; the hazard is exactly the one
`register_phase57_process`' own comment spells out for the three `ProcessHandle`
triples it shares with `register_p60_process_handle`.

## 3. `Process.descendants()` answered for the wrong process

The largest find, and it is a `--jdk-only` defect that exists **because** wave 3
succeeded — the same causal shape W3-6 itself opened with.

`is_vm_process`' doc comment said *"every concrete `java.lang.Process` native
now asks"* and then named five: `isAlive`, `pid`, `toHandle`,
`destroyForcibly`, `waitFor(long, TimeUnit)`. That was a list, not a census.

Taken properly, with `javap -p java.lang.Process` and
`javap -p java.lang.ProcessImpl` (JDK 25.0.3, Windows image, this host), because
the question is not "is the method concrete" but **"is it concrete AND
unoverridden by the class strict mode actually instantiates"**:

| registered on `java/lang/Process` | on the image | `ProcessImpl` overrides | guard needed |
| --- | --- | --- | --- |
| `waitFor()I`, `exitValue`, `destroy`, the three stream getters | abstract | yes (must) | no — dispatch never walks up |
| `isAlive`, `pid`, `toHandle`, `destroyForcibly`, `waitFor(JLjava/util/concurrent/TimeUnit;)Z` | concrete | yes | yes, and all five have it |
| `onExit` | concrete | **yes** | no, for the same reason as the abstract rows |
| `descendants` | concrete | **NO** | **yes — and it had none** |

`java.lang.ProcessImpl` on the Windows image overrides `exitValue`, `waitFor()`,
`waitFor(long, TimeUnit)`, `destroy`, `onExit`, `toHandle`,
`supportsNormalTermination`, `destroyForcibly`, `pid`, `isAlive`, the three
stream getters and `toString`. `descendants()` is not among them. Neither are
`info()` and `children()` — but this VM does not register those, so they reach
the JDK's own bytecode and are fine.

So under `--jdk-only`, where `ProcessBuilder.start()` returns a real
`java.lang.ProcessImpl`, `p.descendants()` walks up to `java/lang/Process` and
lands on `native_process_descendants` with a receiver whose layout is the JDK's.
`PROC_FIELD_PID` is `JAVA_PROCESS_FIELD_COUNT + 4` = slot **10**; a
`ProcessImpl` has six inherited fields and five of its own, and slot 10 holds a
stream reference. The read did not even yield a `Long`, fell to the `-1`
default, and `collect_descendant_pids(-1)` returns `Ok(vec![])` from its
`pid <= 0` guard.

**`Process.descendants()` therefore answered an empty stream for a process with
a live subtree.** Not an error path, not a failed scan — a *correct scan of the
wrong process*, which is precisely the one route the `ProcessScanError`
machinery W6-10 finding 4 built cannot see. `Ok(vec![])` is a truthful report
about pid -1.

The fix is the JDK's own concrete body, verbatim: for a receiver that is not one
of the VM's own `Process` objects, `return toHandle().descendants();`, which
reaches the real `ProcessHandleImpl.descendants()` and therefore
`getProcessPids0` — the same probe, for the right pid. A `toHandle()` that
refuses propagates its `UnsupportedOperationException`, because that is how the
spec spells the refusal and an empty stream is not.

### The RED, and the check that had to be thrown away to get it

The obvious assertion is the identity the JDK's own body states — that
`live.descendants()` and `live.toHandle().descendants()` report the same pids —
and it is **wrong**. It was written, run against real HotSpot 25 on this host,
and rejected by the oracle before any CratonVM arm existed:

```
Exception in thread "main" java.lang.AssertionError:
  Process.descendants() must agree with ProcessHandle.descendants(): 1 vs 2
```

Two separate reads of the live OS process table, microseconds apart, of a
`cmd.exe` that is spawning a `ping`. This is the same trap
`L10-rjdkprocess-vector-overassertion.md` recorded for this very file, and
`awaitInTree` already carries the fix; the new check simply did not inherit it.
**Worth stating as its own finding: an assertion derived from the spec can still
be unsound against the spec's own implementation, and the cheap way to learn
that is to run it on the oracle first.** Building the VM was never needed for it.

What landed instead polls for a non-empty answer, bounded by `TREE_WAIT_MS`,
because what the defect produces is *always* empty:

```java
if (windows()) {
    check(awaitOwnDescendant(live), "Process.descendants() must see the sleeper's own child");
} else {
    skip("Process.descendants(): the Unix sleeper execs, so it has no descendant to see");
}
```

On this host the sleeper is `cmd.exe /c ping -n 30 127.0.0.1`, which forks a
real `ping` grandchild that outlives the whole 10s window, so the check is
genuinely exercised here. On Unix `/bin/sh -c "sleep 30"` normally execs and has
no grandchild to see — a non-empty assertion would fail on HotSpot too — so it
is a **named** `skip`, not a silent absence. That is the machinery from §1 being
used for the thing it was built for on the day it was built.

Before the fix, `awaitOwnDescendant` returns false after 10s and the vector
fails with `Process.descendants() must see the sleeper's own child`. **Stated,
not observed: the CratonVM arms were not run in this lane.**

### The oracle run that WAS done

`javac` + `java` on the fixture against
`C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot`, three times:

```
CK RJdkProcess checks=55 skipped=[]
PASS RJdkProcess (55 checks)
```

identical on all three. So `EXPECTED_CHECKS = 55` is **measured on the oracle,
not counted by hand** — which matters, because a ratchet seeded from a
hand-count is a gate that fails for its own arithmetic on the first run. The
empty `skipped` list is the second half of it: on HotSpot every guard is true
and every check is a real assertion, which is the baseline the CratonVM arms are
diffed against.

## 4. `ProcessImpl.create` fabricated a handle for a command line it could not parse

Both early returns in `native_process_impl_create` answered
`Ok(Some(Value::Long(0)))`, and **the JDK caller has no null-handle check**:
`ProcessImpl.<init>` stores whatever `create` returned into `handle` and goes
straight on to `getProcessId0(handle)`. So `ProcessBuilder.start()` handed back
a live `java.lang.ProcessImpl` naming no process — `pid()` 0, `isAlive()` false,
`waitFor()` -1 — where HotSpot 25 raises
`IOException: Cannot run program "": CreateProcess error=87`.

The reachable arm is the second: `new ProcessBuilder("").start()` reaches
`ProcessImpl.createCommandLine` with an empty program name, so `cmdstr` is empty
and `tokenize_command_line` yields nothing.

`create` is declared `throws IOException` on the image
(`javap -p -s java.lang.ProcessImpl`), so an `IOException` is the answer
`ProcessBuilder.start()`'s own `catch` is already written for. Both arms throw
now. `RJdkProcess` gained the check beside the existing missing-executable one,
which is a *different* code path — that name reaches the OS and is refused
there, this one does not survive command-line assembly.

This is W6-10 finding 4's species entering through the **argument** path of code
whose **syscall** path was already fixed. The species does not stay fixed by
fixing one path; that is now the second recorded instance of the same sentence.

## 5. W6-10 — finding 2's shape survived on the next native along

W6-10 finding 2 removed a double `OpenProcess` from `info0`: it asked
`start_time_or_any(pid)` and then `os_process_cpu_nanos(pid)`, and on Windows
both bottom out in `win_process_times`, so one `ProcessHandle.info()` opened the
same process twice and read a different half of the same struct each time.

`native_proc_handle_is_alive0` had the identical shape and was not looked at:

```rust
None => Ok(Some(Value::Long(if foreign_pid_is_alive(pid) {   // OpenProcess #1
    start_time_or_any(pid)                                   // OpenProcess #2
} else { -1 }))),
```

`foreign_pid_is_alive` is `OpenProcess` + `GetExitCodeProcess` + `CloseHandle`;
`start_time_or_any` is `OpenProcess` + `GetProcessTimes` + `CloseHandle`. Two
opens for one question — and it is not only cost. `win_process_times`' own doc
comment states the invariant *"one `OpenProcess` answers both quantities, so
they can never disagree about which process they describe"*, and this caller
broke it in the one native whose whole return value exists to let
`ProcessHandleImpl.isAlive()` and `destroy0` detect a **recycled pid**. A probe
that hunts pid recycles must not itself straddle one.

Merged into `win_liveness_and_start_time(pid) -> (bool, Option<i64>)`: one
handle, `GetExitCodeProcess` then `GetProcessTimes`, same
`PROCESS_QUERY_LIMITED_INFORMATION` mask (both queries accept it, so no extra
right is requested), `CloseHandle` once. Every arm is one of the two old
functions' arms unchanged — the DWORD-range refusal that keeps
`ProcessHandle.of(Long.MAX_VALUE)` syscall-free, the `ERROR_ACCESS_DENIED`
"exists but we lack rights" edge, the 259 ambiguity HotSpot also inherits, and
the degradation to `STARTTIME_ANY` when `GetProcessTimes` fails. The
`#[cfg(windows)] foreign_pid_is_alive` wrapper is deleted rather than kept: it
would have no caller, and leaving a boolean-only door onto the same probe is how
the pairing grows back.

`foreign_start_time_or_dead` is the seam. Its `not(windows)` arm is the old
two-step verbatim, because on Linux those two probes read two *different* files
(`/proc/<pid>` for existence, `/proc/<pid>/stat` for the start time) and merging
them is a separate change on an arm this host cannot compile. Recorded, not
attempted.

### The free half of the remaining cost

`native_proc_handle_get_process_pids0` probed all three array lengths **per
enumerated process** — `~3N` virtual `array_length` calls per snapshot, and
`~6N` across the JDK caller's mandatory retry (`ProcessHandleImpl` sizes its
arrays at 100 and re-invokes whenever the count exceeds that). They are
loop-invariant: `set_array_element` cannot allocate, resize, or move a ref.
Hoisted, and the walk now breaks once every array is full instead of iterating
the tail for its side-effect-free branches.

The **expensive** half is deliberately unchanged and still inside the loop:
`start_time_or_any` is one `OpenProcess`/`GetProcessTimes`/`CloseHandle` per
emitted row on Windows, `PROCESSENTRY32` carries no creation time, and W6-10
finding 3's conclusion that there is no cheaper documented source stands.
Paying it only for indices the caller's array can hold is what keeps the first
(100-element) pass at 100 opens rather than `N`.

### What is NOT claimed, and what the orchestrator must run

**No speedup is claimed. Nothing was built, nothing was timed, and no counter
was read.** What is claimed is a syscall count, provable by reading:

| operation, Windows | `OpenProcess` before | after |
| --- | --- | --- |
| `ProcessHandleImpl.isAlive0(foreign pid)` | 2 | **1** |
| `ProcessHandle.of(pid)` (one `isAlive0`) | 2 | **1** |
| `ProcessHandle.isAlive()` on a foreign handle | 2 | **1** |
| `ProcessHandle.info()` | 2 | 2 (unchanged — W6-10 finding 2 already) |
| `getProcessPids0`, `allProcesses()` | `100 + N` | `100 + N` (unchanged) |

`isAlive0` on this VM's **own** child is unchanged and makes zero `OpenProcess`
calls either way: it answers from the process table.

To confirm, in this order:

1. `cargo test -p cratonvm-native-io process` — the new
   `one_open_reports_the_same_start_time_as_the_separate_probe` is the guard
   that matters. It asserts the merged probe's start time is **identical** to
   `os_process_start_time`'s, which is not a tautology: the instant `isAlive0`
   sources its number from a different probe than `start_time_or_any`,
   `Info.info(pid, startTime)`'s bare `!=` wipes the record and W5-2's two
   skipped checks come straight back.
2. `cargo test -p cratonvm-native-io` and `cargo test -p cratonvm-native-builtins`
   in full. **Not `cargo check --all-targets`** — it runs no tests.
3. `regression-suite/run.sh` for `RJdkProcess` on all three arms. The falsifying
   observation is below.
4. Only then, if a number is wanted: `--stack-sample-ms` over a workload that
   actually calls `ProcessHandle.of` / `isAlive` in a loop. There is no such
   workload in the suite, which is the honest reason this lane priced the change
   in syscalls rather than in time. Do not run a microbenchmark inline in `main`
   — OSR will refuse it (see the interpreter records).

## The single falsifying observation

```
PASS RJdkProcess (55 checks)
CK RJdkProcess checks=55 skipped=[]
```

on **all three** arms — HotSpot 25, `--real-jdk`, `--jdk-only`, byte-identical.
The HotSpot half of that is not a prediction: it was run three times in this
lane and printed exactly those two lines each time.
Anything else falsifies something specific, and each failure names itself:

* `checks=` any other number, or an `AssertionError: check count moved` — a
  check stopped running. That was W5-2's entire failure mode and it is now loud.
* a non-empty `skipped=[…]` on a CratonVM arm — `info().command()` or
  `info().startInstant()` regressed to empty. Legal, reported, and a visible
  diff against HotSpot's empty list.
* `Process.descendants() must agree with ProcessHandle.descendants(): 0 vs 1` —
  §3 is not fixed.
* `starting an empty program name must raise IOException` — §4 is not fixed.
* an `UnsatisfiedLinkError` naming any `ProcessImpl` or `ProcessHandleImpl`
  member — W3-6's census was incomplete after all.

## Compatible mode (`--real-jdk`), per change

Contractually byte-for-byte frozen except for genuine HotSpot-parity bug fixes.
Stated per change:

| change | reaches Compatible? | justification |
| --- | --- | --- |
| `ProcessImpl.create` throws instead of answering handle 0 | **no.** In Compatible mode `ProcessBuilder.start` is shadowed by `native_process_builder_start` and the image's `ProcessImpl.create` never runs | n/a |
| `Process.descendants()` guard + delegation | **yes**, the native is registered in both modes | Compatible mode's `start()` shadow means the receiver is always a `cratonvm/synthetic/Process`, for which `is_vm_process` is true and the old path is taken **unchanged**. The new branch is reachable only from a receiver Compatible mode does not produce |
| `isAlive0` single `OpenProcess` | **yes** | HotSpot parity, and the Java-visible value is identical arm for arm: the pair `(alive, start)` is what the two calls produced whenever no pid recycle intervened, and where one did the old answer was wrong. `ProcessHandleImpl.isAlive()` reads only the sign and the comparison against `this.startTime`, both unchanged |
| `getProcessPids0` length hoist | **yes** | no observable change of any kind — the values written, the count returned, and the order are identical. A pure loop-invariant hoist |
| `RJdkProcess` fixture | it is a test, not VM behaviour | the vector is tightened, never weakened: two guards that silently skipped now report, and the count is an assertion rather than a print |

No existing test was weakened or deleted. `is_vm_process`' doc comment was
corrected rather than trimmed — its five-item list is now a seven-row census
with the two rows it was missing and the reason each does or does not need the
guard.

## Platform, stated per change

Windows is this host. The platform split matters and no "X-only" claim is made
that this lane could not check:

* `win_liveness_and_start_time`, `foreign_start_time_or_dead`'s `windows` arm,
  and the `ProcessImpl.create` throws — **Windows only**, compilable here in
  principle.
* `foreign_start_time_or_dead`'s `not(windows)` arm — **NOT compilable here**,
  and kept to a three-line body that is the previous code verbatim, for exactly
  the reason W6-10 gives: a type error in an arm the driving host never compiles
  is invisible until someone else builds.
* the `descendants()` guard and the `getProcessPids0` hoist — **platform-neutral**,
  outside every `#[cfg]`.
* the Linux double-read (`/proc/<pid>` then `/proc/<pid>/stat` for one
  `isAlive0`) is the same shape as §5 and is **recorded, not fixed**. It is
  cheaper there — two file reads, not two handle opens — but the pid-recycle
  attribution hole is identical, and HotSpot's `ProcessHandleImpl_unix.c` reads
  ppid and start time from one `/proc/<pid>/stat`. A lane on a Linux host should
  take it.

## The rule this lane is an instance of

W5-2 closed its defect and left its *detector* exactly as it found it. The
detector was a printed integer, and a printed integer is evidence only for as
long as someone is reading it. Every one of the three new defects here is
something a passing run said nothing about: an empty stream, a `Process` that
named no process, a doubled syscall. The generalisable move is not "sweep for
the shape again" — it is **convert the observation that caught it into an
assertion**, so the next instance fails instead of printing.

Corollary, and it is what made §3 findable at all: a comment that says *"every
X now does Y"* and then enumerates is a list, not a census. Re-derive it from
the image before trusting it. The five it named were right; the sixth existed.
