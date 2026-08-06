# `Runtime.exec` returned a Process with no streams — FIXED 2026-08-06

**Status:** FIXED. Retired from
`docs/known-issues/tomcat/testsecurity2019-cgi-empty-response-body.md`.

**Reproducers:**
`probes/CgiExecProbe.java` (the narrow shape),
`probes/ProcSurfaceProbe.java` (the whole `Runtime.exec` surface, 32 lines,
diffed against HotSpot 25),
`probes/ExecPolicyProbe.java` (the spawn gate),
and the original test:
`org.apache.tomcat.security.TestSecurity2019#testCVE_2019_0232`.

## What the original doc reported

Tomcat's `CGIServlet` runs a CGI script as an external process and copies the
script's stdout into the HTTP response body. The test asserted `200 OK` (which
passed) and then `res.toString().contains("Query string:")`, which threw NPE
because `res` — a `ByteChunk` — was empty and `ByteChunk.toString()` answers
`null` for an unwritten chunk on HotSpot too.

The doc had already ruled out a `ByteChunk` null-vs-empty bug by direct probe,
and suspected "CratonVM's CGI output capture (stdout piping from the child
process)". That was the right neighbourhood. It was not `Process` stream
plumbing that was broken, though — that plumbing was fine and was never
reached.

## Root cause

`CGIServlet` spawns via `Runtime.getRuntime().exec(String[], String[], File)`.

CratonVM registers natives for all six `Runtime.exec` overloads
(`native-builtins/src/lib.rs`), and every one of them funnelled into
`lang_system::runtime_spawn_process`, which did **not** use the VM's real spawn
path. It ran `std::process::Command::output()` and stored the child's whole
stdout and stderr as two Java `String`s on a three-slot object allocated with
`alloc_concurrent_synthetic(ctx, "java/lang/Process", 3)`.

Three defects came out of that one function.

**1. The object had nowhere to put those slots.** In real-JDK mode
`java.lang.Process` is a real loaded class with six fields of its own, so
`alloc_concurrent_synthetic` produced a real-layout, six-slot object. The
requested three extra slots did not exist, and the writes landed on
`java.lang.Process`'s own reader/writer caches.

**2. Nothing downstream spoke that layout.** `Process.getInputStream()` and its
siblings are answered by `native-io/src/process.rs`, which reads an
fd-carrying layout that begins *past* those six real fields
(`PROC_FIELD_STDOUT_FD` is slot 8, `PROC_FIELD_HANDLE` is slot 11). Reading slot
8 of a six-slot object is out of bounds; the heap guard dropped it and the
getter fell through to a pipe stream on fd `-1`, whose every read returns EOF.
So a `Runtime.exec` child's output was simply unreachable, and Tomcat copied
nothing into the response. The guard was saying so out loud the whole time:

```text
WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped
  index=8 num_slots=6 class_id=ClassId(1994) class_name=java/lang/Process
  real_field_count=Some(6)
```

**3. `exec` blocked until the child exited.** `Command::output()` waits.
`Runtime.exec` must return immediately with a running child, and `CGIServlet`
depends on exactly that: its "replacement for `Process.waitFor()`" loop polls
`proc.exitValue()` and reads `IllegalThreadStateException` as "still running".
A child that reads its own stdin could never be fed either, because the caller
only receives the pipe once `exec` has returned.

`rc == 200` with an empty body is what all three produce together: the servlet
saw a stream that was at EOF from the first read, and an `exitValue()` that
never threw, so it completed the request normally with nothing to write.

## The fix

`runtime_spawn_process` now delegates to
`cratonvm_native_io::process::spawn_and_wrap` — the same spawn
`ProcessBuilder.start` uses. The `std::process::Child` stays alive in the
process table, its three pipes are registered in the `FdTable`, and the object
handed back is a `cratonvm/synthetic/Process` in the layout every
`java.lang.Process` native already reads.

Two smaller divergences on the same natives were fixed while the function was
open, both measured against HotSpot:

* A **null element in the cmdarray was dropped**, so
  `exec(new String[] {"/bin/echo", null})` silently ran a *different* command
  line than the caller wrote. `ProcessBuilder.start` throws
  `NullPointerException`; an empty cmdarray throws
  `ArrayIndexOutOfBoundsException` (that is `cmdarray[0]` failing). Both now
  match — `probes/ProcSurfaceProbe.java` T10/T11.
* The **working-directory `File` was read from slot 0 only**. In real-JDK mode
  `java.io.File`'s slot 0 is not necessarily `path`, so the directory the
  caller asked for was silently dropped. It reads `path` by name first now,
  the same order `native-io::process::file_path_of` uses.

## Residual found and fixed: the spawn gate covered only one entry point

Chasing this surfaced a second, independent defect on the same surface.

`SecurityManager.checkExec` is documented in this repo as the gate for both
`Runtime.exec` and `ProcessBuilder.start` (see the comments in
`native-builtins/src/security_manager.rs`). It was applied by `Runtime.exec`
and by `native-builtins`' own `ProcessBuilder.start` — but *that*
`ProcessBuilder.start` is shadowed at runtime by `native-io`'s, which had no
gate at all. So a deny-all policy refused `Runtime.exec` and let
`new ProcessBuilder(...).start()` fork the same child unchallenged:

```text
before   EXEC_REFUSED_AS=SecurityException   EXEC_CHILD_RAN=false
         PB_REFUSED_AS=NOT_REFUSED           PB_CHILD_RAN=true
after    EXEC_REFUSED_AS=SecurityException   EXEC_CHILD_RAN=false
         PB_REFUSED_AS=SecurityException     PB_CHILD_RAN=false
```

The gate moved into `spawn_and_wrap`, which every spawn route funnels through
(`ProcessBuilder.start`, `Runtime.exec`, `ProcessImpl.create`, `forkAndExec`).
`native-io` cannot reach the SecurityManager — `native-builtins` depends on it,
not the reverse — so it is installed as a `fn`-pointer hook
(`set_spawn_policy_hook`) at registration time. The hook is process-global
because it is the same code in every VM; it resolves the *calling* VM's
SecurityManager through the `NativeContext` it is handed, so nothing per-VM is
latched. `runtime_spawn_process` no longer calls `check_exec_or_throw` itself,
so the SecurityManager is still consulted exactly once per spawn.

## Why no test caught it

`org.apache.catalina.servlets.TestCGIServletCmdLineArguments` (18 tests) passed
on the broken build, before and after — it asserts on status codes and on the
*absence* of the injected file, never on the response body. The one assertion
in the whole suite that read a CGI body was the line that NPE'd.

`native-io`'s own tests exercised `spawn_and_wrap` correctly. Nothing tested
that `Runtime.exec` *reached* it.

## Verification

| | |
|---|---|
| `probes/ProcSurfaceProbe.java`, 32 lines | byte-identical to HotSpot 25, in default AND `--jdk-only` mode |
| `probes/CgiExecProbe.java` | `VERDICT=CGI_OK`, header + 33-byte body (was 0 headers, 0 bytes) |
| `probes/ExecPolicyProbe.java` | both entry points refused, neither child forked |
| `TestSecurity2019` | `OK (3 tests)` — HotSpot control `OK (3 tests)` |
| `TestCGIServletCmdLineArguments` | `OK (18 tests)`, unchanged |
| `TestOpenSSLCipherConfigurationParser` (also uses `Runtime.exec`) | `OK (73 tests)`, unchanged |
| `cargo test -p cratonvm-native-io --lib` | 405 passed, 0 failed |
| `cargo test -p cratonvm-native-builtins --lib` | 3308 passed, 0 failed |
| `cargo check --all-targets`, default AND `synthetic-jdk` | clean |

`cargo test -p cratonvm-vm --lib` is 2443 passed / 4 failed — the same four
`dev` already fails (`runtime_error_array_index_carries_index`,
`hot_files_have_no_production_panics`, and two `resolve::guard` allowlist rows
whose offender is `native_override.rs`'s `find_method_recursive` count). This
branch touches none of those files.

### The regression test, with its negative control

`lang_system::exec_cmdarray_tests::runtime_exec_returns_while_the_child_is_still_running`
spawns `/bin/sleep 5` through `native_runtime_exec_array` and asserts the call
returns in under 2.5s. Reverted to the old `Command::output()` body it fails
with `took 5.001257109s`; with the fix it returns in ~0ms. The bound is loose
on purpose — the claim is "returned before the child finished", not a latency
budget, and this runs on a shared, loaded host.

`process::tests::spawn_policy_hook_is_consulted_and_can_refuse_the_fork` checks
both arms in one test, deliberately: the hook is a `OnceLock`, so a second test
installing a different one would silently keep the first.

`process::tests::spawn_and_wrap_exposes_a_live_stdout_pipe` pins the property
the CGI body depended on, but it is honest to say it would NOT have caught this
bug — `spawn_and_wrap` was always correct. The route into it was the defect,
which is what the timing test above covers.

## What to take away

An `alloc_concurrent_synthetic(ctx, "<a real JDK class>", n)` is a silent
truncation whenever the real class has fewer than `n` fields, and in real-JDK
mode `java/lang/Process` does. Every write past the real field count is
dropped, every read past it returns nothing, and the only trace is a `WARN`
from the heap guard that reads like a speculative-probe false positive. Two
more callers allocated a Process this way —
`phases_late::register_phase57_process`'s `ProcessBuilder.start` and
`lang_system`'s `native_pb_start` — dead only because `native-io` registers
over them later. This section originally left them in place, on the grounds
that removing them would change what happens if the registration order ever
moved. See the follow-up below: they were removed the same day, and taking them
out exposed a third defect underneath.

## Follow-up 2026-08-06: the other two callers, and what removing them exposed

The section above ended by naming two more callers that allocate a Process
under the real class name `java/lang/Process` and were left in place because
they are dead. They are gone now, and so is the third layout nobody mentioned.

### Both `ProcessBuilder.start` reimplementations are now the same function

`native-io`'s `native_process_builder_start` is `pub` and registered from all
three sites. It was already a strict superset of the other two — it honours
`redirectInput/Output/Error` and `redirectErrorStream`, reads a `List` through
the List API when it is not ArrayList-shaped, and returns live pipes — and it
already won every dispatch, since `register_io_natives` runs after
`register_essential_natives_with_shims` and `register()` is
last-registration-wins. Registering the same function pointer three times makes
the redundancy real: whichever registration wins, the behaviour is identical.

Deleted with them:

* `phases_late`'s legacy Process layout (`JAVA_PROCESS_FIELD_COUNT` + four
  `PROC_FIELD_*` slots) and the **sixteen** Process natives that read it —
  `waitFor`, `exitValue`, `destroyForcibly`, `pid`, `toHandle`,
  `getInputStream`, `getErrorStream`, `getOutputStream`, each on both
  `java/lang/Process` and `cratonvm/synthetic/Process`. Every one of those
  triples is registered by `native-io` on the same two class names against the
  live child. What they *would* have answered if the ordering moved was worse
  than nothing: they read pid from slot 9, which in the surviving layout is the
  stderr file descriptor.
* `lang_system`'s `native_pb_start` — a `ProcessBuilder.start` that asked the
  SecurityManager for permission and then returned a one-slot dummy Process
  that had spawned nothing — and `lib.rs`'s five `java/lang/Process` natives
  reading slot 0 of a *third* layout, whose comments still described the
  `Command::output()` spawn model that stopped being true when `native-io`
  took over spawning.
* `native-io`'s `captured_string_stream` / `legacy_captured_stream`, the
  `getInputStream` path that re-wrapped those captured Strings. With no
  producer left it was reading slot 7 of every Process hoping to find a String,
  and slot 7 is now an `Int`.

Net: −687 lines, +390.

### One coincidence made load-bearing

`ProcessBuilder.environment()` stored its map only at the indexed slot 2, while
`start()` looks it up by the *name* `environment`. That worked solely because a
real JDK 25 `java.lang.ProcessBuilder` declares `command`, `directory`,
`environment` in that order, so slot 2 *is* the named field. `environment()`
now writes both spellings (as the `command` registrations already did), and
`read_process_environment` falls back to slot 2 when no named field exists.

### What the verification probe found: `ProcessHandle.isAlive()` never went false

Writing `probes/ProcHandleProbe.java` to check that nothing regressed turned up
a defect that predates all of this — confirmed by building `origin/dev`
unmodified and running the same probe:

```
HotSpot   AFTER proc_exists=false handle.isAlive=false process.isAlive=false
dev       AFTER proc_exists=false handle.isAlive=true  process.isAlive=false
```

`Process.isAlive()` was right and `ProcessHandle.isAlive()` on a handle from
the same child was wrong, for two independent reasons in one native:

1. **It was keyed by the wrong identifier.** `ProcessHandleImpl`'s four natives
   are handed a **pid** by the JDK's own bytecode — `isAlive0(pid)`,
   `waitForProcessExit0(pid, ..)`, `destroyProcess0(pid, ..)`,
   `destroy0(pid, startTime, ..)` — and all four passed it straight into
   `try_exit_handle` / `wait_for_handle` / `destroy_handle`, which are keyed by
   `NEXT_HANDLE`, a counter starting at 1. Two unrelated number spaces, so the
   lookup always missed: `isAlive0` said "still running" for every pid forever,
   `waitForProcessExit0` returned −1 without waiting, `destroyProcess0` killed
   nothing. A comment on `native_process_to_handle` had already named this and
   deferred it ("fixing that needs the table to also be queryable by real pid").
   It is queryable now — `handle_for_pid`.

2. **The return value is not a boolean.** `ProcessHandleImpl.isAlive()` reads
   it as the process's *start time*:

   ```java
   long startTime = isAlive0(pid);
   return startTime >= 0
       && (startTime == this.startTime || startTime == 0 || this.startTime == 0);
   ```

   Any value ≥ 0 means alive; −1 is the only way to say "not alive". The native
   returned 1 for running and **0 for exited** — and 0 is ≥ 0. With
   `this.startTime == 0` on every handle `build_process_handle` mints, the third
   disjunct then made the answer `true` unconditionally. The two errors could
   not cancel: the answer was "alive" either way.

Both fixed; `ProcHandleProbe` is now byte-identical to HotSpot 25.

`process::tests::process_handle_wait_for_exit_enters_gc_blocked_region` had to
be repaired rather than satisfied — it passed the internal handle where the JDK
passes a pid, so it was asserting the bug.

### Verification

| | |
|---|---|
| `probes/ProcHandleProbe.java` | byte-identical to HotSpot 25 (was 1 line divergent on pristine `dev`) |
| `probes/ProcSurfaceProbe.java`, 32 lines | still byte-identical, default AND `--jdk-only` |
| `probes/CgiExecProbe.java` | `VERDICT=CGI_OK`, 33-byte body |
| `probes/ExecPolicyProbe.java` | both entry points refused, neither child forked |
| `TestSecurity2019` / `TestCGIServletCmdLineArguments` / `TestOpenSSLCipherConfigurationParser` | `OK (3)` / `OK (18)` / `OK (73)` |
| `cargo test -p cratonvm-native-io --lib` | 407 passed, 0 failed — 5 consecutive runs, no flake |
| `cargo test -p cratonvm-native-builtins --lib` | 3315 passed, 0 failed |
| `cargo check --all-targets`, default AND `synthetic-jdk` | clean, no warnings |

The `spawn_policy_hook` test needed a `SPAWN_TEST_LOCK`: the hook is
process-global once installed, so its deny arm refused an unrelated test's
spawn. That surfaced as a flaky `SecurityException` from
`spawn_and_wrap_exposes_a_live_stdout_pipe`, which is worth recording — any
future test that spawns through `spawn_and_wrap` needs that lock.

### What is left

Nothing allocates a Process under a real JDK class name any more, and there is
exactly one Process layout with exactly one writer. The general landmine stands
though, for other classes: `alloc_concurrent_synthetic(ctx, "<a real JDK class
name>", n)` silently truncates whenever the real class has fewer than `n`
fields, and reports it only as a `cratonvm::gc::guard` WARN whose own text
blames a speculative collection probe.
