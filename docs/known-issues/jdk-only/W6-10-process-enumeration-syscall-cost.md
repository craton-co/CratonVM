# Windows process enumeration: one snapshot per tree node, and one `OpenProcess` too many per `info()`

<!-- merge: both sides kept; the lane's finding and the reconciliation's commit attribution are complementary -->
**Status: FINDINGS 1, 2 AND 4 CONFIRMED PRESENT 2026-08-12 (W7-46). One
follow-up, and it is finding 2's own shape one native along.**

`collect_descendant_pids` takes one snapshot and indexes it by parent;
`start_time_and_cpu` fetches both halves from one `win_process_times`; every
enumeration primitive returns `Result` and the three natives that reach them
throw. All three are in the tree and readable.

**Finding 2 was not swept far enough.** `native_proc_handle_is_alive0` had the
identical double-`OpenProcess` shape — `foreign_pid_is_alive(pid)` and then
`start_time_or_any(pid)`, two handle opens for one question — in the one native
whose return value exists so that `ProcessHandleImpl.isAlive()` and `destroy0`
can detect a **recycled pid**. A probe that hunts pid recycles must not itself
straddle one, which is this record's own argument for `info0` applied to its
neighbour. Merged into `win_liveness_and_start_time`: one handle,
`GetExitCodeProcess` + `GetProcessTimes`, same desired-access mask, every arm
unchanged. Windows only; the non-Windows arm is the old two-step verbatim,
because there the two probes read two different `/proc` files and merging them is
a change on an arm this host cannot compile.

**Linux followed on 2026-08-12** (`linux_liveness_and_start_time`, one
`/proc/<pid>/stat` read, sharing its parser with `linux_proc_stat_times`), from a
Windows host and therefore **still uncompiled**. Only the
`not(any(target_os = "linux", windows))` arm keeps the two-step now, correctly:
there `os_process_start_time` has no probe and answers `None`, so there is no
second read to straddle a recycle with. Full reasoning, including the arm that
must NOT collapse "the line will not parse" into "dead", in
W7-46-process-cluster.md §8.1.

**Three of this record's own claims are re-checked below and one is stale**;
read the 2026-08-12 block before quoting a cost.

Finding 3's conclusion stands unchanged — `PROCESSENTRY32` carries no creation
time, so the per-row `OpenProcess` in `getProcessPids0` is inherent. Its *free*
half was taken: the three `array_length` probes were loop-invariant and are now
hoisted, and the walk stops once every array is full.

**Still no measurement.** Costs are stated in `OpenProcess` counts, as in the
rest of this record. What the orchestrator must run to confirm, and what is
explicitly not being claimed, is in W7-46-process-cluster.md.

The two structural costs landed 2026-08-07; finding 4 — a snapshot failure
indistinguishable from an empty machine — on 2026-08-11. Lane W6-10 of the
jdk-wave2 pool.
**Files changed:** `native-io/src/process.rs`.
**Status (reconciled 2026-08-12 — W7-55-record-reconciliation.md):**

* **All four findings: CLOSED in source.** Finding 1 (one snapshot for the whole
  `descendants()` walk, `1+D` → `1`, `O(N·D)` → `O(N+D)`) — the Windows arm of
  `collect_descendant_pids` indexes a single `os_snapshot_processes` by parent
  pid. Finding 2 (`start_time_and_cpu(pid)`, one `OpenProcess` instead of two) —
  present in `native-io/src/process.rs`, with the catch-all arm narrowed to
  `#[cfg(not(any(target_os = "linux", windows)))]`. Finding 3 is a no-op claim:
  the remaining `OpenProcess` cost is **inherent** because `PROCESSENTRY32`
  carries no creation time. Finding 4 (a snapshot failure indistinguishable from
  an empty machine) landed 2026-08-11 as commit `278688257` *fix(process): a
  failed process enumeration must throw, not report an empty machine* — the
  `ProcessScanError` return channel, five widened signatures, three natives
  throwing `java.lang.RuntimeException`. Downstream corroboration:
  `p60_delegate_to_real_handle`'s doc comment at
  `native-builtins/src/phases_late.rs:2433-2439` explicitly relies on that
  `RuntimeException` propagating untouched.
* **Residual: STILL OPEN — the out-of-file addition, and it is now
  UN-APPLIABLE AS WRITTEN.** The `## Out-of-file addition (not applied)` below
  asks for a sixth row in the table of
  `W2-7-fabricated-success-where-the-spec-mandates-failure.md`. That record no
  longer lives in this directory — it was retired on 2026-08-11 into the
  internal fixed-bugs tree as
  `jdk-only-W2-7-fabricated-success-where-the-spec-mandates-failure-FIXED-20260811.md`,
  and that file's table stops at row 4. Whoever picks this up must decide where
  the row now belongs; do not go looking for the original path.
* **Cannot adjudicate without a run — two, one needing a different host.**
  1. The 2026-08-11 change has not been built or run. The record's claims are
     stated in syscall counts and asymptotics, provable by reading; the only
     falsifier it names is a profile of `Process.descendants()` on a deep tree.
  2. The `not(any(linux, windows))` arms were changed to return `Err` and are
     **not compilable on this dev host** — type-correctness there is settled
     only by the advisory macOS CI job (`.github/workflows/cross-platform.yml`).
     See "Which arm could not be compiled".

Lane W6-10 of the jdk-wave2 pool. **Files changed:** `native-io/src/process.rs`.

This lane is a follow-up on a cost that this campaign introduced. Wave 3
(`W3-6-processimpl-missing-natives.md`) gave Windows real process enumeration —
`parent0`, `getProcessPids0` and `descendants` had been Linux-only, answering
`-1` and the empty list, so `ProcessHandle.parent()` was permanently
`Optional.empty()` and `children()`/`descendants()`/`allProcesses()` were
permanently empty streams. It built that on a
`CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS)` reader and closed with a note:

> Windows `getProcessPids0` now costs one `OpenProcess` per enumerated process.

The lane could not run a benchmark, so everything below is stated in **syscall
counts and asymptotics**, provable by reading the code, not in time.

## Notation

* `N` — processes visible on the machine (the size of one Toolhelp snapshot).
* `K` — rows that survive the caller's filter (for `children()`, the direct
  children of one pid).
* `D` — descendants of a pid, i.e. the size of the subtree below it.

## Re-check, 2026-08-12 (W7-46 follow-up) — which findings still describe code that runs

Asked because a record that prices a change is worthless if the code no longer
runs. Source-only, like the rest of this record.

| finding | premise today |
|---|---|
| 1 | **STALE under `--jdk-only`.** See the block appended to Finding 1 below. |
| 2 | **LIVE and unaffected.** `ProcessHandle.info()` reaches `native_proc_handle_info0` in both shipping modes. |
| 3 | **LIVE, and reached MORE than when this was written** — the delegation in finding 1's note routes strict-mode `descendants()` through `getProcessPids0`. |
| 4 | **LIVE** for `parent0` and `getProcessPids0`. For `native_process_descendants` it now guards only the VM-receiver (Compatible-mode) path. |

Finding 4's **five widened signatures were audited on every arm** — the thing
this record flagged as unverifiable from this host. All five are consistent:
identical parameter types, arity and return type on every `cfg` arm, with every
caller matching, and `direct_child_pids`' missing `windows` arm exactly matched
by its sole caller being `not(windows)`. The table is in
W7-46-process-cluster.md §8.4. **That is a source read, not a compile**: it rules
out a `Result` widened on one arm only, a drifted arity, and a caller left
unwrapping a bare value; it cannot rule out a borrow or inference error inside a
body. The Linux build is still owed.

## Finding 1 — `Process.descendants()` took one machine-wide snapshot per node

`collect_descendant_pids` is a breadth-first walk that expanded each node with
`direct_child_pids(cur)`. On Windows that resolved to
`os_list_processes(cur)` -> `os_snapshot_processes()`: a **full**
`CreateToolhelp32Snapshot` plus `N` `Process32NextW` steps, filtered down to the
one parent it cared about, with the other `N - k` rows discarded.

| | snapshots | work |
|---|---|---|
| before | `1 + D` | `O(N * D)` |
| after | `1` | `O(N + D)` |

The snapshot already carries `th32ParentProcessID` for every row — the whole
parent/child topology is in the **first** call. The fix indexes that one
snapshot by parent pid and walks it in memory. The Linux arm is deliberately
left alone: its per-node probe is `/proc/<pid>/task/*/children`, a direct read
of the node being expanded rather than a machine-wide scan, so there is no
re-enumeration to hoist.

Output is unchanged, not merely equivalent: `by_parent[cur]` holds exactly the
rows `os_list_processes(cur)` selected (`ppid == cur`) in the same snapshot
order, so the breadth-first sequence is identical. It still derives from
`os_snapshot_processes`, the single source `os_parent_pid` and
`os_list_processes` also read, so `Process.descendants()` and
`ProcessHandle.descendants()` cannot report different trees.

Scope: this is `java.lang.Process.descendants()`, registered on
`java/lang/Process` (keycloak-test-framework's `ProcessUtils.getKeycloakPid()`
is the known consumer). `ProcessHandle.descendants()` is real JDK bytecode that
makes a single `getProcessPids0(0, ...)` call and builds the tree in Java, so it
never had this shape.

> **STALE under `--jdk-only` since 2026-08-12, and stale because of a fix in
> W7-46-process-cluster.md §3.** `Process.descendants()` on a real
> `java.lang.ProcessImpl` receiver now returns `toHandle().descendants()` — the
> JDK's own concrete body — so it reaches `getProcessPids0` and never touches
> `collect_descendant_pids`. This optimised Windows walk is now entered **only**
> for a `cratonvm/synthetic/Process` receiver, i.e. Compatible mode.
>
> The direction is worth stating, because it is a cost this campaign ADDED and
> nobody had priced. `collect_descendant_pids` reads the whole topology out of
> one Toolhelp snapshot and `build_process_handle` stamps `startTime` 0 without
> probing, so the old path was **1 snapshot, 0 `OpenProcess`**. The JDK route is
> `getProcessPids0(0, …)` plus `ProcessHandleImpl`'s mandatory 100-element
> retry: **2 snapshots, `100 + N` `OpenProcess`**. Strictly more expensive, and
> strictly more correct — the old path answered for the WRONG PROCESS. Do not
> revisit the fix; do stop quoting `1 + D` → `1` as a live saving in strict
> mode, and note that finding 3's per-row `OpenProcess` is now the dominant cost
> of this whole surface.

## Finding 2 — `info0` opened the same process twice for one `GetProcessTimes`

`native_proc_handle_info0` asked for `start_time_or_any(pid)` and then
`os_process_cpu_nanos(pid)`. On Windows **both** bottom out in
`win_process_times`, which is one `OpenProcess` + `GetProcessTimes` +
`CloseHandle` returning *both* quantities. So a single `ProcessHandle.info()`
opened the same process twice and read a different half of the same struct each
time.

| | `OpenProcess` per `info()` on Windows |
|---|---|
| before | 3 (start time, CPU time, image name) |
| after | 2 (times, image name) |

The third open is `QueryFullProcessImageNameW` and is genuinely a different API;
it stays.

This was not only a wasted syscall pair. `win_process_times`' own doc comment
states the invariant "one `OpenProcess` answers both quantities, so they can
never disagree about which process they describe" — and the caller broke it: two
separate opens means a pid recycled between them yields a start time from one
process and a CPU total from another. Fetching them together restores the
invariant the primitive was written to guarantee.

### The start-time agreement is preserved by construction

Wave 5 (`W5-2-two-silently-skipped-process-checks.md`) established that
`ProcessHandleImpl$Info.info(pid, startTime)` — real JDK bytecode — wipes
`command`, `arguments`, `startTime`, `totalTime` and `user` off the record it
just filled unless `startTime != info.startTime` is false. That is a bare `!=`
with none of the `STARTTIME_ANY` wildcarding `equals()`/`isAlive()` apply, so
`isAlive0`, `info0` and `ProcessHandle.current()` must all report the **same
number**. Wave 5 routed all three through one `start_time_or_any(pid)`.

The new `start_time_and_cpu(pid)` does not add a second source:

* Windows arm — `win_process_times(pid)` mapped to its first element with a
  `PROCESS_STARTTIME_ANY` fallback. That is literally `os_process_start_time`'s
  body plus `start_time_or_any`'s `unwrap_or`, inlined; it cannot differ.
* non-Windows arm — calls `start_time_or_any(pid)` outright.

`isAlive0` and `current_process_start_time` are untouched and still call
`start_time_or_any`. All three remain one function's answer.

## Finding 3 — the note was accurate, and the remaining `OpenProcess` cost is inherent

`native_proc_handle_get_process_pids0` calls `start_time_or_any` per emitted
row, and on Windows that is one `OpenProcess`/`GetProcessTimes`/`CloseHandle`.
`PROCESSENTRY32` carries `th32ProcessID`, `th32ParentProcessID`, `cntThreads`,
`pcPriClassBase` and `szExeFile` — **no creation time**. There is no cheaper
documented source, so the handle is unavoidable for any row whose start time the
caller will actually read. Two things are already right and were left alone:

* the parent/child topology needs **no** handle — `th32ParentProcessID` is read
  straight out of the snapshot by `os_parent_pid` and `os_list_processes`, so
  `parent0` costs one snapshot and zero `OpenProcess`;
* the handle is opened only for rows that survive the cheap filters. For
  `children()` (`of_pid != 0`) the snapshot filter runs first, so the cost is
  `K` opens for `K` children, not `N`. For `allProcesses()` (`of_pid == 0`) every
  row becomes a `new ProcessHandleImpl(pid, startTime)` on the Java side, so `N`
  really are needed. The write is additionally skipped for indices past the
  caller's array length.

One cost that reading the JDK's own caller makes visible and that no native
change can remove: `ProcessHandleImpl.children()`/`allProcesses()` sizes its
arrays at 100 and **retries** whenever the returned count exceeds that. On a
machine with `N > 100` processes, `allProcesses()` therefore takes 2 snapshots
and `100 + N` opens, the first 100 of whose results the caller discards.

## Finding 4 (FIXED 2026-08-11) — a snapshot failure was indistinguishable from an empty machine

`os_snapshot_processes` **returned** `Vec<(i64, i64)>` and returned it **empty** when
`CreateToolhelp32Snapshot` failed (null or `INVALID_HANDLE_VALUE`) or when
`Process32FirstW` failed. Its callers were infallible, so:

* `getProcessPids0` reported `0` found -> `ProcessHandle.allProcesses()` is an
  empty stream, `children()` is an empty stream;
* `parent0` reported `-1` -> `ProcessHandle.parent()` is `Optional.empty()`.

Those are exactly the answers a machine running nothing and a process with no
parent would produce. This is the campaign's dominant defect species — a
fabricated success where the spec mandates a failure — and it is the *same*
shape wave 3 removed from the default arms, re-entering through the error path.

HotSpot does not do this. `ProcessHandleImpl_md.c` (Windows) throws
`java.lang.RuntimeException` when `CreateToolhelp32Snapshot` returns
`INVALID_HANDLE_VALUE` and again when `Process32First` fails;
`ProcessHandleImpl_unix.c` throws `RuntimeException` when `opendir("/proc")`
fails. `allProcesses()` being *restricted* is documented and legal; a list
*truncated by an error* is a different thing and is not.

### What landed

A `ProcessScanError` — a struct carrying one message string — is now the return
channel every enumeration primitive in `process.rs` uses to say "the syscall did
not run", and it is a *different value* from `Ok(vec![])` / `Ok(-1)`, which is the
whole of the fix. Every signature that used to answer with the ambiguous value:

| function | `cfg` arms | before | after |
|---|---|---|---|
| `os_snapshot_processes` | windows | `Vec<(i64,i64)>` | `Result<Vec<(i64,i64)>, ProcessScanError>` |
| `os_parent_pid` | linux, windows, other | `i64` | `Result<i64, ProcessScanError>` |
| `os_list_processes` | linux, windows, other | `Vec<(i64,i64)>` | `Result<Vec<(i64,i64)>, ProcessScanError>` |
| `direct_child_pids` | linux, other | `Vec<i64>` | `Result<Vec<i64>, ProcessScanError>` |
| `collect_descendant_pids` | windows, not(windows) | `Vec<i64>` | `Result<Vec<i64>, ProcessScanError>` |

and the three natives that reach them now throw instead of answering:

* `native_proc_handle_get_process_pids0` (`ProcessHandleImpl.getProcessPids0`) —
  `allProcesses()` / `children()`;
* `native_proc_handle_parent0` (`ProcessHandleImpl.parent0`) — its `_ctx`
  parameter had to become a real `ctx`, since throwing needs one;
* `native_process_descendants` (`java.lang.Process.descendants()`).

`os_parent_pid`'s Linux arm is `Result` for the signature's sake only and never
returns `Err`: `ProcessHandleImpl_unix.c` throws for a failed `opendir("/proc")`
but its `parent0` returns `-1` for a process it cannot read, because one
unreadable `/proc/<pid>/status` means that process is gone, not that procfs is.
The uniform return type is what keeps `parent0` a single un-`cfg`'d body. The
same reasoning keeps a failed `/proc/<pid>/task` read inside `direct_child_pids`
an `Ok` empty expansion — a descendant that dies mid-walk must not abort the
walk.

**The exception.** `RuntimeError` (`types/src/error.rs`) still has no plain
`RuntimeException` variant, and the objection to `IllegalStateException` stands:
it is a *subclass*, so it is catchable by the same `catch` but has the wrong
`getClass()`. Rather than add a variant in a file this lane does not own, the
throwable is constructed directly — `new_object` + pinned `<init>` +
`MethodCallFailed::ExceptionThrown` — which is the mechanism this crate already
uses for every exception class `RuntimeError` cannot spell (`stream_decoder.rs`'s
`UnsupportedEncodingException`, `socket_channel.rs`'s `java.nio.channels`
family, `nio_native.rs`'s `FileAlreadyExistsException`). No new mechanism, and
no out-of-file change. `IllegalStateException` survives only as the fallback for
a run in which `java/lang/RuntimeException` itself cannot be constructed.

The message names the syscall and carries the OS error
(`std::io::Error::last_os_error()` on Windows, the `io::Error` from `read_dir` on
Linux) because "the snapshot failed" alone cannot be triaged from a log:
`ERROR_ACCESS_DENIED` and `ERROR_BAD_LENGTH` — the latter a snapshot taken while
the process table churns, which a caller may legitimately retry — are the same
sentence without it. On the `Process32FirstW` path the error is captured
*before* `CloseHandle`, which succeeds and would otherwise reset the thread's
last-error.

`Process32FirstW` failing is now thrown on unconditionally, `ERROR_NO_MORE_FILES`
included. That is what HotSpot does, and a Toolhelp snapshot always contains at
least the System process, so there is no honest empty case being suppressed.

### The `not(any(linux, windows))` arms changed too, deliberately

`os_list_processes`, `os_parent_pid` and `direct_child_pids` all had a
platform-of-last-resort arm answering the empty list / `-1`. That is the same
fabrication wave 3 removed from Windows *by implementing it*, and it is not
oracle-faithful either: HotSpot has a `sysctl(KERN_PROC_ALL)` implementation on
macOS/BSD and answers a real tree there. Those arms now return `Err` naming the
missing probe, so `allProcesses()` / `parent()` / `descendants()` throw on such a
host instead of claiming a machine with nothing running.

This is a **behaviour change on a platform this campaign does not run**, and it
is worth being explicit about: the macOS CI job is compile-only and advisory
(`.github/workflows/cross-platform.yml`, `continue-on-error: true`), so nothing
measured changes. The correct end state is a real arm, not a thrown exception;
until someone writes it, the exception is the honest report.

### Related shapes left as-is deliberately

The enumeration loop ends
on the first `Process32NextW` that returns FALSE without checking for
`ERROR_NO_MORE_FILES`, so a mid-walk failure would silently truncate. A Toolhelp
snapshot is a frozen copy, so `ERROR_NO_MORE_FILES` is the only realistic
terminator, and HotSpot's loop has the identical shape — diverging here would be
stricter than the oracle.

## The rest of the file's error paths, swept

Finding 4 is the shape "an OS failure is reported as a legitimate answer". Every
other primitive in `process.rs` that can fail was re-read for it. Only the
enumeration family was defective; the rest degrade to a value the JDK *itself*
defines as "unknown", which is a different thing and is what the oracle does.

| primitive | on failure | verdict |
|---|---|---|
| `win_process_times` (`OpenProcess` + `GetProcessTimes`) | `None` -> `start_time_or_any` -> `STARTTIME_ANY` (0) | **OK.** 0 is `ProcessHandleImpl`'s own "exists, start time unavailable", distinct from `STARTTIME_PROCESS_UNKNOWN` (-1), and every comparison against it is short-circuited. HotSpot's `getStatInfo` likewise leaves the fields unset. |
| `os_process_image_name` (`QueryFullProcessImageNameW`) | `None` -> `Info.command` left null | **OK.** Every `Info` field is an `Optional`; `Optional.empty()` is the JDK's word for "not available", and HotSpot's Windows `getCmdlineInfo` does exactly this when the query fails. |
| `foreign_pid_is_alive` (`GetExitCodeProcess`) | `queried && code == STILL_ACTIVE`, so a failed query reads as NOT alive | **OK, and deliberate.** `isAlive0` has no throwing contract — its whole vocabulary is a start time or `-1`. Note the asymmetry is already handled one level up: `ERROR_ACCESS_DENIED` from the `OpenProcess` is reported *alive*, because that means the process exists and we lack rights. |
| `os_process_start_time` / `boot_time_millis` / `clock_ticks_per_second` (`/proc/<pid>/stat`, `/proc/stat`) | `None` -> `STARTTIME_ANY` | **OK.** Same as `win_process_times`; `ProcessHandleImpl_unix.c` returns `-1` from `os_getParentPidAndTimings` and its callers treat it as unknown rather than throwing. |
| `os_process_cmdline` (`/proc/<pid>/cmdline`) | `None` -> `command`/`arguments` left null | **OK.** Same `Optional` reasoning. |
| `start_time_matches` | an unreadable start time counts as a MATCH | **OK, documented at the site.** "Unknown is not disagreement" — the alternative is refusing to signal a live child because `/proc` was momentarily unreadable. |

The one that is a judgement call rather than a clear pass is
`foreign_pid_is_alive`: a `GetExitCodeProcess` that fails on a handle
`OpenProcess` just returned is reported as "not alive", which is a fabricated
*negative*. It is left alone because the native's return type cannot express
anything else and HotSpot's cannot either — recorded here so the next reader does
not have to re-derive it.

## Which arm could not be compiled

Nothing was built for this change (the lane is source-only), but the distinction
worth stating is which arm this dev host could not have compiled **even in
principle**, because a prior lane in this campaign was caught rewriting `cfg`
arms the driving host never compiles (commit 7f59676f3, "the sweep rewrites cfg
arms the driving host never compiles"):

* **Windows** — the host's own target. Compilable here in principle.
* **`target_os = "linux"`** — NOT compilable on this host. `os_parent_pid`,
  `os_list_processes`, `direct_child_pids` and the `not(windows)`
  `collect_descendant_pids`. **Three more joined them on 2026-08-12**, from a
  lane also on Windows: `linux_liveness_and_start_time` (new),
  `linux_stat_line_times` (the parse half of `linux_proc_stat_times`, split out)
  and the new `target_os = "linux"` arm of `foreign_start_time_or_dead`. Same
  mitigation, applied again: no borrowed state, no new control flow beyond one
  three-arm `match` on `std::fs::read_to_string`'s `Result`, and the split
  parser is the OLD body moved unchanged. Also a Linux-only `#[test]`,
  `one_stat_read_reports_the_same_start_time_as_the_separate_probe`, which is
  the mirror of the Windows one this record's confirmation list already names.
* **`not(any(target_os = "linux", windows))`** — NOT compilable on this host, and
  not on the Linux fixture host either. `os_parent_pid`, `os_list_processes`,
  `direct_child_pids`.

Mitigation, since a type error there is invisible until someone else builds: the
three uncompilable arms were kept as trivial as the change allows. Every one of
them is either a `return Ok(x)` where a bare `x` used to be, or a single
`Err(ProcessScanError::new(..))` body with no borrowed state; the only arm with
real new control flow (the captured-then-`CloseHandle` ordering on the
`Process32FirstW` failure) is the Windows one. `ProcessScanError::new` is
constructed on all three platforms, so no arm leaves it `dead_code`.

## Out-of-file addition (not applied — and the target has MOVED)

> **CLOSED — NO TARGET, 2026-08-12 (W7-46 §8.3). Do not carry this forward.**
>
> Two corrections to the reconciliation note that stood here. It said the
> retired target's *"table stops at row 4"*: it does not — that file's table has
> a **row 5**, `ProcessHandleImpl.isAlive0` in `native-io/src/process.rs`,
> marked FIXED, which is this very family filed from the other end. And the row
> this section asks for would be a sixth entry in the species inventory of a
> record that is **retired and internal**, describing a defect that is **fixed**
> and already recorded in two live records (finding 4 above, and
> W7-46-process-cluster.md §2). It documents nothing a reader of this directory
> can act on.
>
> Kept below as drafted text, for the day the species inventory is re-founded as
> a live record. It is not work.
>
> **Reconciled 2026-08-12 (superseded, kept for provenance).** Still unapplied,
> and no longer appliable as written. The target record was retired out of this
> directory on 2026-08-11 into the internal fixed-bugs tree as
> `jdk-only-W2-7-fabricated-success-where-the-spec-mandates-failure-FIXED-20260811.md`.
> Decide where the row belongs before writing it; do not go looking for the path
> quoted below.

The inventory for this defect species was `W2-7-fabricated-success-where-the-spec-mandates-failure.md`,
which was not this lane's file to edit. Its table should gain a row:

| # | Symptom | Spec answer | Site | Disposition |
|---|---------|-------------|------|-------------|
| 6 | A failed process enumeration (`CreateToolhelp32Snapshot`/`Process32First`/`opendir("/proc")`) reported as an empty machine — `allProcesses()`/`children()`/`descendants()` empty, `parent()` `Optional.empty()` | `java.lang.RuntimeException` | `native-io/src/process.rs::os_snapshot_processes` and its callers | FIXED 2026-08-11 (W6-10 finding 4) |

Worth noting for the inventory's own argument: this instance entered through the
ERROR path of code that wave 3 had *already* fixed on its default path. The
species does not stay fixed by fixing the happy path.

## Not changed, and why

`os_parent_pid` (Windows) materialises the whole snapshot `Vec` and then
`find`s one row. That is `O(N)` either way, it is one snapshot per
`ProcessHandle.parent()` call, and it is not called in any loop on the Windows
arm (only the Linux `os_list_processes` calls `os_parent_pid` per entry, and
there it is a file read). Trimming the allocation would be a constant-factor
guess of exactly the kind this lane is barred from making without a profile.

## What would falsify this

The claims are all structural, but one observation would overturn the headline:
if `java.lang.Process.descendants()` is in practice only ever called on a
childless or one-level process (`D` of 0 or 1), Finding 1's `1 + D` -> `1` is a
saving of at most one snapshot and the asymptotic framing is real but idle. The
`O(N * D)` -> `O(N + D)` statement is still correct; its *value* is only
realised on a deep tree, and the known consumer (a `kc.sh` wrapper that execs a
JVM) is two levels deep.
