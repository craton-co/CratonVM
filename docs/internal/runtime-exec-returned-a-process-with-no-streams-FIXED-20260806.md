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
more callers still allocate a Process this way —
`phases_late::register_phase57_process`'s `ProcessBuilder.start` and
`lib.rs`'s `native_pb_start` — and both are dead only because `native-io`
registers over them later. They were left in place (removing them would change
what happens if the registration order ever moves), but they are the same
landmine, and the next one will look exactly like this one: a `200 OK` with
nothing in it.
