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

> ## Follow-up pass, 2026-08-12 (lane W7-46b) — the two recorded-not-fixed rows
>
> Both are now dispositioned; §8 below is the whole of it. In one line each:
>
> * **The Linux `isAlive0` double-read is FIXED IN SOURCE**, on an arm this host
>   still cannot compile — `linux_liveness_and_start_time` is now the single
>   door, shaped exactly like `win_liveness_and_start_time`, and the parser it
>   shares with `linux_proc_stat_times` is what keeps the two start times one
>   number. **NOT BUILT, NOT RUN, and it must go to a Linux host.**
> * **The four double-registered `java/lang/ProcessBuilder` triples are
>   ADJUDICATED, not fixed** — the fix is out of lane, and §8.2 carries the
>   exact deletion. Re-measured rather than restated: the collision costs a
>   `SyntheticStub` → **`Intrinsic`** kind rewrite on three of the four, which is
>   worse than this record originally supposed, and `start()` is not one of them.
> * The **inventory row inherited from W6-10** has no target left and is closed
>   as such, not carried forward — §8.3. W6-10's claim that the retired W2-7
>   record's "table stops at row 4" is **wrong**: it stops at row 5, and row 5 is
>   this very family.
> * W6-10's finding 4 — five signatures widened across arms nobody compiled —
>   was audited on **every** arm. All five are consistent; §8.4 has the table.
> * W6-10's finding 1 has gone **stale in `--jdk-only`** through this record's own
>   §3 fix, and in the expensive direction. §8.5.

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
| `p60_unmeasurable_process_tree` | empty stream | **synthetic-JDK only, and refused under strict**, where the `SyntheticStub` tag drops the registration that reaches it. **MEASURED 2026-08-12 (lane A31): CONFIRMED LIVE — and it is a fabricated success, not an "unmeasurable" answer.** See §9 |
| `p60_empty_optional` on the five `$Info` accessors | `Optional.empty()` | **the specified answer**, not a concession — the JDK's own accessors derive exactly this from an unwritten field |

### Two-gate `#[cfg]` with a default-off inner gate: **none**

`native-io/src/process.rs` contains no `feature = …` gate at all — every `#[cfg]`
in it is `windows` / `target_os = "linux"` / `not(any(…))`, and the three arms
partition the target space with no default-off hole. No `TODO`, `FIXME`,
`todo!()` or `unimplemented!()` anywhere in the file. The documented hole
species is absent from this surface; that is a measurement, not an assumption.

### One shape recorded, not fixed — out of lane

> **Re-measured 2026-08-12 — §8.2.** Two claims below are too gentle. The
> untagged block inherits **`Intrinsic`** (a *chosen* kind, so it rewrites the
> slot's kind, not merely its callback), and it wins **three** of the four, not
> all four — `start()` is re-won afterwards by `native-io`'s
> `register_process_natives`. §8.2 carries the exact deletion.

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

`foreign_start_time_or_dead` is the seam. Its `not(windows)` arm was left as the
old two-step verbatim, because on Linux those two probes read two *different*
files (`/proc/<pid>` for existence, `/proc/<pid>/stat` for the start time) and
merging them is a change on an arm this host cannot compile.

**That was taken on 2026-08-12 — §8.1.** The seam now has three arms rather than
two, the Windows and Linux bodies are textually identical below the probe name,
and only the platform-of-last-resort arm still takes two steps (correctly: there
`os_process_start_time` answers `None` outright, so there is no second read to
straddle a recycle with).

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
  `isAlive0`) is the same shape as §5 and was **recorded, not fixed**. It is
  cheaper there — two file reads, not two handle opens — but the pid-recycle
  attribution hole is identical, and HotSpot's `ProcessHandleImpl_unix.c` reads
  ppid and start time from one `/proc/<pid>/stat`. A lane on a Linux host should
  take it. **Taken in source 2026-08-12 (§8.1) by a lane that is still on
  Windows, so the "should" is only half discharged: what is left is a
  `cargo build --target x86_64-unknown-linux-gnu` (or a Linux host) and
  `cargo test -p cratonvm-native-io process`.**

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

---

> **VERIFIED AGAINST A BINARY 2026-09-03.** Status was **SOURCE-ONLY**, *"No
> CratonVM binary was built or run."* The Rust half now has been:
>
> ```text
> cargo test -p cratonvm-native-io process     25 passed; 0 failed
> ```
>
> Including the rows this record's changes are about — the confinement gate, the
> spawn-policy hook, the GC-blocked regions around every wait, and destroy
> reaching a child a waiter is parked on:
>
> ```text
> process::tests::validate_spawn_program_confinement_gate                   ok
> process::tests::spawn_policy_hook_is_consulted_and_can_refuse_the_fork    ok
> process::tests::process_wait_for_enters_gc_blocked_region                 ok
> process::tests::process_wait_for_timeout_enters_gc_blocked_region_between_polls  ok
> process::tests::foreign_receiver_timed_wait_also_enters_a_blocked_region  ok
> process::tests::destroy_reaches_a_child_a_waiter_is_blocked_on            ok
> process::tests::a_real_pid_resolves_to_its_table_handle                   ok
> ```
>
> **This record's own good habit is worth restating, because it is why the
> discharge is small.** It ran the modified `RJdkProcess.java` against real
> HotSpot before shipping — *"A fixture change is testable on the oracle alone;
> not doing so is a habit worth losing"* — which is what seeded `EXPECTED_CHECKS`
> from a measurement and caught its own unsound `descendants()` check. The
> oracle side was therefore never the debt. Only the CratonVM side was, and
> `RJdkProcess` is in the regression suite run alongside this note.
>
> **What this does NOT verify.** The `javap` census against the Windows JDK 25
> image is a source-and-oracle count and was not re-taken. The three further
> defects the sweep found are described in this record's own sections and are
> not individually re-adjudicated here — the unit tests above cover the
> `native-io/src/process.rs` surface, not every claim in the sweep.

# 8. The follow-up pass, 2026-08-12 — clearing the two recorded-not-fixed rows

**Still SOURCE-ONLY. No CratonVM binary was built or run, no `javac`, no
`cargo`.** Everything below is provable by reading, and where it is not, it says
so at the point it is claimed. The lane host is Windows, and **the substantive
change is on Linux**, which is the whole difficulty and is stated per item.

## 8.1 The Linux `isAlive0` double-read — FIXED IN SOURCE, UNCOMPILED

`foreign_start_time_or_dead`'s `not(windows)` arm was `foreign_pid_is_alive(pid)`
— an `exists()` on `/proc/<pid>` — followed by `start_time_or_any(pid)`, which
reads `/proc/<pid>/stat`. Two probes of two different paths for one question, in
the native whose return value exists so `ProcessHandleImpl.isAlive()` and
`destroy0` can detect a **recycled pid**.

The Windows shape was ported rather than re-invented, and the port is smaller
than the original because there is no handle to own:

| | before | after |
|---|---|---|
| `isAlive0(foreign pid)`, Linux | `exists("/proc/<pid>")` + `read("/proc/<pid>/stat")` | **one** `read("/proc/<pid>/stat")` |
| the pair `(alive, start)` | two reads, two instants | one read, one line |

Three things are worth stating because each was a decision, not a transcription:

1. **The READ and the PARSE are now separate functions.** `linux_proc_stat_times`
   keeps its name and becomes read-then-`linux_stat_line_times`; the new
   `linux_liveness_and_start_time` does its own read and calls the same parser.
   Collapsing them into "the parse failed, therefore dead" would have been
   shorter and **wrong**: `linux_stat_line_times` also answers `None` when
   `/proc/stat` carries no `btime` or `_SC_CLK_TCK` is 0 — machine-wide
   conditions that say nothing about the process — and a live process would then
   be reported DEAD. The old code degraded those to `STARTTIME_ANY` and so does
   this one. That is the "`(true, None)` when the file opened but the line will
   not parse" arm.
2. **`ENOENT` is the liveness answer, and only `ENOENT`.** `/proc/<pid>/stat` is
   mode 0444, so a live process cannot be missing it; any *other* error is
   reported `(true, None)` rather than dead, which is the same asymmetry the
   Windows arm makes for `ERROR_ACCESS_DENIED`. A zombie still reads as alive,
   because `/proc/<pid>/stat` survives until the process is reaped — that is what
   the deleted `exists("/proc/<pid>")` knew, preserved.
3. **It is the oracle's own shape.** `ProcessHandleImpl_unix.c`'s `isAlive0` is
   one `os_getParentPidAndTimings` — a single `/proc/<pid>/stat` open — returning
   `-1` when it fails. So this is HotSpot's structure, not a local invention;
   the divergence was ours.

`#[cfg(target_os = "linux")] fn foreign_pid_is_alive` is **deleted**, on the same
argument the Windows one was: it would have no caller, and a boolean-only door
onto the same probe is how the pairing grows back. A tombstone comment stands
where it was. The `not(any(target_os = "linux", windows))` arm keeps both the
function and the two-step, correctly — there `os_process_start_time` has no probe
at all and answers `None`, so there is no second read to straddle a recycle with.

### What covers it, and the check that could NOT be written

**No `RJdkProcess` assertion was added, and that is a finding rather than an
omission.** What the merge removes is a pid-recycle attribution window and a race
between two `/proc` reads. Neither is provokable from a vector: to observe the
old code answering wrongly you must exit a process between its two reads and have
the pid recycled onto another, which no test can arrange on demand. Every
assertion this lane could think of would have passed on the OLD behaviour too,
and a check that cannot fail is the exact species `W6-5-vacuous-tests.md`
catalogues. The rule this directory runs on — *"every assertion you add must fail
on the old behaviour"* — refused it, so it was not written.

What IS assertable is the invariant the merge must not break, and it is asserted
where its Windows twin already is: `native-io/src/process.rs`'s test module gains
`one_stat_read_reports_the_same_start_time_as_the_separate_probe`, a
`#[cfg(target_os = "linux")]` mirror of
`one_open_reports_the_same_start_time_as_the_separate_probe`. It asserts that
`linux_liveness_and_start_time`'s start time is **identical** to
`os_process_start_time`'s, which is not a tautology: the moment the two parse
field 22 differently, `Info.info(pid, startTime)`'s bare `!=` wipes the record and
W5-2's two silently-skipped checks come straight back. It is a scheduled test —
`cargo test -p cratonvm-native-io process` is the command this record already
names — and, being Linux-only, **it has never run**.

There is a second reason `RJdkProcess` was left alone, and it is a triage reason:
this directory's own index records that the vector may not be back to its
expected count and that **a control binary pre-dating the 2026-08-12 merges fails
it identically**. Moving `EXPECTED_CHECKS` from a Windows host with no build
would put a fresh arithmetic failure on top of a pre-existing one.

## 8.2 The four `java/lang/ProcessBuilder` triples — ADJUDICATED; the fix is out of lane

Re-measured against the registrars rather than restated, and **this record's
original framing was too gentle on two counts**.

The four triples — `<init>(Ljava/util/List;)V`, `<init>([Ljava/lang/String;)V`,
`command()Ljava/util/List;`, `start()Ljava/lang/Process;` — are registered by:

| registrar | file | reached from | kind |
|---|---|---|---|
| `register_phase57_process` | `native-builtins/src/phases_late.rs:1340` | `register_essential_natives_with_shims` (`lib.rs:9356`) — **every mode** | `SyntheticStub`, stated |
| the untagged block in `register_enterprise_natives` | `native-builtins/src/lib.rs:37971-37986` | `register_synthetic_overrides` (`lib.rs:23905`) — synthetic-JDK mode **only** | **ambient `Intrinsic`** |
| (`start` only) `register_process_natives` | `native-io/src/process.rs`, near the end | `register_io_natives` — **every mode, and LAST** | `SyntheticStub`, stated |

**Correction 1 — the ambient kind is not "no opinion".**
`register_synthetic_overrides` opens with
`registry.set_category(NativeKind::Intrinsic)` and `register_enterprise_natives`
sets no category of its own, so the four inherit **`Intrinsic`**. That matters
because `NativeMethodRegistry::register_inner`'s kind-merge rule keeps a prior
*chosen* kind only against a registration that **expressed no opinion**, and
`set_category` counts as choosing. So this is not a callback-only shadowing: it
**rewrites the slot's kind from `SyntheticStub` to `Intrinsic`**, and `Intrinsic`
is precisely the kind `CompatibilityMode::JdkOnly` does *not* drop. The whole
point of `register_phase57_process`' `SyntheticStub` restatement is undone for
three of the four, in the one mode where it runs.

**Correction 2 — `start()` is NOT one of the affected triples.** `register_io_natives`
runs after `register_builtins` on both arms of `vm_init` (`vm_init.rs:1837-1840`
for the synthetic arm), and it re-registers `ProcessBuilder.start` with an
explicit `NativeKind::SyntheticStub`. So `start` is re-won and re-tagged after the
collision, in every mode. Three triples are live, not four.

**What it costs, in synthetic-JDK mode.** Behaviourally, little: `PB_FIELD_COMMAND`
is `0` (`phases_late.rs:1270`) and `native_pb_init` writes slot 0, so the only
lost write is the `set_field_by_name(this, "command", …)` that phases_late also
does — and on a fabricated `ProcessBuilder` there is no named field for it to
reach. **The damage is the kind rewrite and the duplicate itself**: three triples
whose whole reason for carrying a stated `SyntheticStub` now carry `Intrinsic` in
the census, and a latent trap — the day `register_enterprise_natives` becomes
reachable from a shipping mode, three `Intrinsic` registrations survive
`--jdk-only` and `native_pb_init` writes **slot 0 of a real
`java.lang.ProcessBuilder`** with no layout witness of any kind.

**The fix is a deletion, and it is the shape that already landed for
`SecureRandom` on 2026-08-12** (three shadowing registrations deleted from a
synthetic-only registrar; see this index's §2.3 note on `L8`). It is out of this
lane's files.

> ### OUT-OF-FILE PATCH — `native-builtins/src/lib.rs`, in `register_enterprise_natives`
>
> Delete the four registrations at `native-builtins/src/lib.rs:37971-37986`,
> i.e. replace
>
> ```rust
>     // ProcessBuilder + Process (simplified)
>     let pb = "java/lang/ProcessBuilder";
>     registry.register(pb, "<init>", "(Ljava/util/List;)V", native_pb_init);
>     registry.register(pb, "<init>", "([Ljava/lang/String;)V", native_pb_init);
>     registry.register(pb, "command", "()Ljava/util/List;", native_pb_command);
> ```
>
> and the `registry.register(pb, "start", …)` call that follows it (through its
> closing `);`) with:
>
> ```rust
>     // ProcessBuilder: NOT registered here any more.
>     //
>     // These four triples were also registered by
>     // `phases_late::register_phase57_process`, which states `SyntheticStub`
>     // for the whole ProcessBuilder cluster so `--jdk-only` refuses it. This
>     // block is reached only from `register_synthetic_overrides`, which opens
>     // with `set_category(Intrinsic)` — a CHOSEN kind — so it ran LAST in
>     // synthetic-JDK mode and rewrote three of those slots from `SyntheticStub`
>     // to `Intrinsic`, the one kind `JdkOnly` does not drop. `start()` was the
>     // fourth and was re-won afterwards by `native-io`'s
>     // `register_process_natives`, which restates `SyntheticStub`.
>     //
>     // The bodies here were also the weaker pair: `native_pb_init` writes only
>     // slot 0, where phases_late writes the indexed slot AND the real-JDK
>     // `command` field by name. Deleting the duplicate leaves one owner per
>     // triple in every mode. See W7-46-process-cluster.md §8.2.
> ```
>
> **Blast radius.** `register_phase57_process` is reached from
> `register_essential_natives_with_shims`, which `register_essential_natives`
> calls unconditionally and which `register_builtins` therefore reaches in
> synthetic mode too — so its reachability is a strict superset of this block's
> and no mode loses a body. Compatible and `--jdk-only` cannot move by any
> amount: this registrar does not run in either.
>
> **Leftovers, deliberately not prescribed here.** `native_pb_init` and
> `native_pb_command` (`native-builtins/src/lang_system.rs:3443`, `:3452`) then
> have zero callers. The workspace sets `dead_code = "allow"`, so nothing breaks;
> deleting them is a `lang_system.rs` owner's call, not a precondition.
>
> **How to prove it took effect.** `--dump-native-registry` on a
> `--features synthetic-jdk` binary in `--synthetic-jdk` **mode**, before and
> after: the three `java/lang/ProcessBuilder` rows must move from `Intrinsic`
> back to `SyntheticStub`, and their `overwrote=` provenance must go empty. A
> census taken in Compatible mode cannot see this change at all — that is
> `docs/architecture/natives-over-real-jdk-classes.md` §7's scoping trap, and it
> is why `bridge-ratchet.sh` will not move either.

## 8.3 The inventory row inherited from W6-10 — CLOSED, no target

W6-10's `## Out-of-file addition (not applied)` asks for a sixth row in the
inventory table of `W2-7-fabricated-success-where-the-spec-mandates-failure.md`.
That record left this directory on 2026-08-11 and the index reassigned the row
here when `W3-6`/`W5-2` were finally moved.

Two things settle it:

* **W6-10's own description of the target is wrong.** It says the retired
  record's *"table stops at row 4"*. It does not: the retired file's table has a
  **row 5**, and row 5 is `ProcessHandleImpl.isAlive0` in
  `native-io/src/process.rs`, marked FIXED — i.e. the same family the missing
  row would describe, filed from the other end.
* The row would be a sixth entry in the inventory of a species, in a record that
  is **retired and internal**, describing a defect that is **fixed** and already
  recorded twice in live records (W6-10 finding 4 and this one, §2). It documents
  nothing a reader of this directory can act on.

So it is closed as *no target*, not carried forward. If the species inventory is
ever re-established as a live record, the row it wants is the one W6-10 already
drafted — the text is still there — plus the correction that this instance
entered through the ERROR path of code whose default path wave 3 had already
fixed.

## 8.4 W6-10 finding 4's five widened signatures — audited on EVERY arm

Finding 4 widened five signatures to `Result<_, ProcessScanError>` across `cfg`
arms that no lane which edited them has ever compiled. Every arm of every one was
re-read; **all five are consistent, and every caller matches.**

| function | arms present | signature on each | callers |
|---|---|---|---|
| `os_snapshot_processes` | `windows` only | `Result<Vec<(i64,i64)>, ProcessScanError>` | three, all `windows`, all `?` |
| `os_parent_pid` | `linux` / `windows` / `not(any(…))` | `Result<i64, ProcessScanError>` — identical on all three | `native_proc_handle_parent0` (`match`), `os_list_processes`'s Linux arm (`?`) |
| `os_list_processes` | `linux` / `windows` / `not(any(…))` | `Result<Vec<(i64,i64)>, ProcessScanError>` — identical | `native_proc_handle_get_process_pids0` (`match`) |
| `direct_child_pids` | `linux` / `not(any(…))` — **no `windows` arm, correctly** | `Result<Vec<i64>, ProcessScanError>` | only `collect_descendant_pids`'s `not(windows)` arm (`?`) |
| `collect_descendant_pids` | `windows` / `not(windows)` | `Result<Vec<i64>, ProcessScanError>` | `native_process_descendants` (`match`) |

Method, since a source read of an uncompilable arm is only as good as its
method: every `fn` in `native-io/src/process.rs` with more than one `cfg` arm was
extracted and its arms compared textually — eleven such functions, and **no arm
of any of them differs from its siblings by parameter type, arity or return
type**. The `cfg` predicates also partition the target space for each: the only
function with a hole is `direct_child_pids`, whose missing `windows` arm is
exactly matched by its sole caller being `not(windows)`.

This is not a compile. It rules out the failure modes a source read *can* rule
out — a `Result` widened on one arm only, an arity that drifted, a caller left
unwrapping a bare value. It cannot rule out a borrow or an inference error inside
a body. **The Linux arms still need a Linux build; that is the standing item in
this index's §2.6, and this pass adds to it rather than discharging it.**

## 8.5 W6-10's performance premise, re-checked — finding 1 is now STALE under `--jdk-only`

Asked because a record that prices a change is worth nothing if the code no
longer runs. Finding by finding:

| W6-10 finding | premise today |
|---|---|
| 1 — `collect_descendant_pids` took `1 + D` machine-wide snapshots | **STALE in `--jdk-only`, and stale because of §3 of this record.** `Process.descendants()` on a real `java.lang.ProcessImpl` receiver now returns `toHandle().descendants()`, which is JDK bytecode reaching `getProcessPids0`. The optimised Windows walk is reached **only** for a `cratonvm/synthetic/Process` receiver — i.e. Compatible mode. |
| 2 — `info0` opened the same process twice | **LIVE and unaffected.** `ProcessHandle.info()` reaches `native_proc_handle_info0` in both shipping modes. |
| 3 — one `OpenProcess` per enumerated row is inherent | **LIVE, and MORE reached than when it was written** — see below. |
| 4 — a failed enumeration read as an empty machine | **LIVE** for `parent0` and `getProcessPids0`; for `native_process_descendants` it now guards only the VM-receiver path, the foreign path having moved into JDK bytecode. |

The part worth stating plainly, because it is a cost this campaign **added** and
nobody has priced: under `--jdk-only`, `Process.descendants()` on Windows went
from *one Toolhelp snapshot and zero `OpenProcess` calls* — `collect_descendant_pids`
reads the topology straight out of the snapshot, and `build_process_handle`
stamps `startTime` 0 without probing — to the JDK's own route, which is
`getProcessPids0(0, …)` with `ProcessHandleImpl`'s mandatory 100-element retry:
**2 snapshots and `100 + N` `OpenProcess` calls**, where `N` is every process on
the machine. That is a real regression in cost and an unambiguous improvement in
correctness (the old path answered for the wrong process — §3), so it is not a
reason to revisit the fix. It is a reason not to quote finding 1's `1 + D → 1` as
a live saving in strict mode, and a reason finding 3's per-row `OpenProcess` is
now the dominant cost of the whole surface.

**Still no measurement, and none is claimed.** These are syscall counts read off
the code, in the units the rest of this record uses.

## 8.6 What this pass did NOT do

* **Nothing was compiled.** No `cargo`, no `javac`, no CratonVM run. The
  substantive change is on `target_os = "linux"`, which this Windows host cannot
  compile even in principle.
* **`RJdkProcess.java` was not touched** — §8.1 gives both reasons, and the
  second one (a vector already red on a pre-merge control) is the kind that gets
  worse when two lanes edit a count nobody can measure.
* **The `ProcessBuilder` deletion was not applied**, because
  `native-builtins/src/lib.rs` belongs to another lane. §8.2 is the patch.
* **`register_enterprise_natives`' other registrations were not swept** for the
  same shape. `StackTraceElement` has seven triples in the same block and they
  are registered nowhere else in that file, but "nowhere else in that file" is
  not the census that question needs — it needs a `--dump-native-registry` diff,
  which needs a build.

---

# 9. Re-verified 2026-08-12 (record-triage lane, doc-only — nothing built or run)

A source read of today's tree. Line numbers are today's.

## 9.1 §8.2's out-of-file patch has been APPLIED — that row is discharged

`native-builtins/src/lib.rs:38139-38153` now carries the replacement comment
this record wrote, and the four `java/lang/ProcessBuilder` registrations are
gone: the only `java/lang/ProcessBuilder` string left in that file is an
unrelated comment at `:14533`. `native_pb_init` and `native_pb_command` survive
as definitions with **zero callers** (`native-builtins/src/lang_system.rs:3667`,
`:3676`) — exactly the leftover §8.2 said it was deliberately not prescribing,
and harmless under the workspace's `dead_code = "allow"`.

So each of the four triples now has one owner per mode:
`phases_late::register_phase57_process` (`native-builtins/src/phases_late.rs:1343`,
`SyntheticStub` stated) for all four, plus `native-io`'s
`register_process_natives` re-winning `start` last, as §8.2 measured.

**What is discharged is the SOURCE half only.** §8.2's own proof —
`--dump-native-registry` on a `--features synthetic-jdk` binary in
`--synthetic-jdk` mode, the three rows moving from `Intrinsic` back to
`SyntheticStub` with empty `overwrote=` — still has not been run by anyone, and
cannot be seen from Compatible mode. Treat the kind-rewrite claim as repaired in
source and unmeasured.

## 9.2 §8.1 and §3 are present, and the Linux half is still uncompiled

* `linux_liveness_and_start_time` — `native-io/src/process.rs:3041`; its parser
  `linux_stat_line_times` at `:2310`.
* Both `foreign_pid_is_alive` tombstone comments are in place (`:2841` Linux,
  `:2854` Windows); the surviving `foreign_pid_is_alive` at `:3100` is the
  platform-of-last-resort one, as stated.
* `win_liveness_and_start_time` at `:2916`; the three `foreign_start_time_or_dead`
  arms at `:3069`, `:3077`, `:3085`.
* Both mirror tests exist:
  `one_open_reports_the_same_start_time_as_the_separate_probe` (`:5783`) and
  `one_stat_read_reports_the_same_start_time_as_the_separate_probe` (`:5834`).
  The Linux one **has still never run**; that remains the standing item.
* §3's fix is at `:4096-4108` — `invoke_virtual(this, "toHandle", …)` then
  `invoke_virtual(handle, "descendants", …)`, with the
  `UnsupportedOperationException` arm for a refusing `toHandle()`, as written.

## 9.3 Scheduling, per claim

`RJdkProcess` is in `JDKONLY_CLASSES` (`regression-suite/run.sh:119`), and
`EXPECTED_CHECKS = 55` is a constant at `RJdkProcess.java:89` asserted at
`:413`. So §1's ratchet and §3's and §4's falsifiers are **scheduled** and will
run on all three arms. The two that are not scheduled anywhere are the Linux
mirror test (needs a Linux build) and §8.2's registry diff (needs a
`synthetic-jdk` build) — the same two holes this record has carried since §8.

## 9.4 A neighbour that belongs on this cluster's map, not in it

Today's committed strict census — `P1-BASELINE-20260812.md` — lists
`cratonvm/synthetic/Process` as one of the **nine** families that still block
`--jdk-only`, entered as **P1-I** through `Runtime.exec` (all six overloads),
with `ProcessBuilder.start` printed beside it as the **passing control**. That
is the same shared mint this cluster owns:
`native-io/src/process.rs::spawn_and_wrap_with_redirects`.

The repair is in source today and this record's readers should not re-derive it:
`native-builtins/src/lib.rs:14531-14560` re-tags all six `Runtime.exec` overloads
`NativeKind::SyntheticStub`, on the argument that `Runtime.exec` is ordinary
bytecode on the image, so strict drops the shadow and the JDK's own `exec` runs
down to the real `ProcessBuilder.start()`. **Unbuilt, unmeasured**, and its
comment states the general rule this cluster keeps paying for: *a retag that
pins one caller of a shared mint must enumerate the callers*.

---

## 9. `p60_unmeasurable_process_tree` adjudicated in `--synthetic-jdk` — 2026-08-12 (lane A31)

This record is SOURCE-ONLY and says so. The one row in its fabricated-answer
table scoped "synthetic-JDK only" has now been run: a `--features synthetic-jdk`
binary, launched with `--synthetic-jdk`.

**The naming is the problem.** `p60_unmeasurable_process_tree` answers an empty
stream, and the row above justifies it as an honest "unmeasurable". The name
asserts that the process tree could not be determined. Measured, it is asserted
even when the tree is trivially determinable — the probe **holds a live child**
and asks in the same breath:

```
                                  HotSpot 25            --jdk-only            --synthetic-jdk
R processHandle.childrenWithChild children=1            children=1            children=0
                                  descendants=2         descendants=1         descendants=0
                                  childAlive=true       childAlive=true       childAlive=true
```

`childAlive=true` on the same line is the discriminator: the child exists, the
VM spawned it, `Process.isAlive()` confirms it, and `children()` says there are
none. There is no error, no violation, no `UnsupportedOperationException` — the
JDK contract for a platform that cannot enumerate is to throw
`UnsupportedOperationException`, not to answer `Stream.empty()`. A caller that
iterates (`children().forEach(kill)`) reads a clean pass and kills nothing.

That belongs in the `W7-20` species (a refusal laundered into a wrong answer),
not in this record's "correct, and load-bearing" column beside
`p60_empty_optional`. The distinction the table draws for `p60_empty_optional` —
*"the specified answer … the JDK's own accessors derive exactly this from an
unwritten field"* — genuinely applies there and does **not** apply here.

Two neighbouring rows are honest absences by comparison, and are recorded so
nobody re-derives them:

```
--synthetic-jdk:
  R processHandle.allProcesses ! java.lang.NoSuchMethodError:
      java.lang.ProcessHandle.allProcesses()Ljava/util/stream/Stream;
  R processHandle.ofPid.self   ! java.lang.NoSuchMethodError:
      java.lang.ProcessHandle.of(J)Ljava/util/Optional;
--jdk-only: allProcesses=316 / present=true
```

Green in `--synthetic-jdk` and needing no further work: `ProcessHandle.current().pid()`
(> 0), `.info().command()` (present), `.parent()` (present),
`ProcessBuilder.start()` itself.

**Verdict: CONFIRMED LIVE, synthetic-mode only, and mis-classified in the table
above.** See lane A31's NOMINATION A31-6. Nothing was changed in
`native-io/src/process.rs` by that lane — it writes no Rust.
