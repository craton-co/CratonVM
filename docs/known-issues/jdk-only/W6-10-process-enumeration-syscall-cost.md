# Windows process enumeration: one snapshot per tree node, and one `OpenProcess` too many per `info()`

**Status:** FIXED for the two structural costs (2026-08-07). Lane W6-10 of the
jdk-wave2 pool. One finding is left OPEN and documented below rather than fixed.
**Files changed:** `native-io/src/process.rs`.

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

## Finding 4 (OPEN, not fixed) — a snapshot failure is indistinguishable from an empty machine

`os_snapshot_processes` returns `Vec<(i64, i64)>` and returns it **empty** when
`CreateToolhelp32Snapshot` fails (null or `INVALID_HANDLE_VALUE`) or when
`Process32FirstW` fails. Its callers are infallible, so:

* `getProcessPids0` reports `0` found -> `ProcessHandle.allProcesses()` is an
  empty stream, `children()` is an empty stream;
* `parent0` reports `-1` -> `ProcessHandle.parent()` is `Optional.empty()`.

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

Not fixed here for two reasons, both about blast radius rather than doubt:

1. `RuntimeError` (`types/src/error.rs`) has no plain `RuntimeException` variant.
   The nearest existing variant, `IllegalStateException`, is a `RuntimeException`
   *subclass* — catchable by the same `catch`, but the wrong `getClass()`, and
   this repo has a standing finding that a subclass is not good enough when the
   spec names a class.
2. Making the answer fallible has to travel through `os_snapshot_processes` ->
   `os_parent_pid` / `os_list_processes` -> `getProcessPids0` / `parent0` /
   `collect_descendant_pids`, across three `cfg` arms each, including the Linux
   arm whose `read_dir("/proc")` failure has the identical shape.

The follow-up is therefore: add `RuntimeError::RuntimeException`, make
`os_list_processes` return `Option<Vec<..>>` on both real arms, and have
`getProcessPids0` throw rather than report `0`.

A related, smaller shape is left as-is deliberately: the enumeration loop ends
on the first `Process32NextW` that returns FALSE without checking for
`ERROR_NO_MORE_FILES`, so a mid-walk failure would silently truncate. A Toolhelp
snapshot is a frozen copy, so `ERROR_NO_MORE_FILES` is the only realistic
terminator, and HotSpot's loop has the identical shape — diverging here would be
stricter than the oracle.

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
