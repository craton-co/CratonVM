# Keycloak empty-stderr process exits

Status: fixed (runner-side; root cause not conclusively internal to CratonVM
— see Investigation). Not reproduced since; harness now retries and
distinctly labels this signature if it recurs.

Date observed: 2026-07-02
Date investigated/mitigated: 2026-07-02

## Summary

The expanded Keycloak `tests` and `testsuite` CratonVM run recorded 2 `CRASH`
rows with process return code `-1` and no captured stdout/stderr content:

```text
tests/base org.keycloak.tests.admin.client.ClientProtocolMapperTest
tests/base org.keycloak.tests.admin.client.ClientProtocolValidationTest
```

Unlike every other crash bucket seen in this suite (including the 338-class
`NamespaceAwareStore.computeIfAbsent` mixed-JUnit-classpath bucket — see
[keycloak-junit-namespaceawarestore-classpath-crashes.md](../known-issues/keycloak-junit-namespaceawarestore-classpath-crashes.md)
— which always produced `rc=1` plus a full diagnostic message), these two
rows had **zero bytes on both stdout and stderr**, despite each process
having run for several real seconds (6.690s and 4.985s respectively) before
dying.

## Representative Rows

```text
index=52
module=tests/base
class=org.keycloak.tests.admin.client.ClientProtocolMapperTest
status=CRASH
rc=-1
seconds=6.690
```

```text
index=53
module=tests/base
class=org.keycloak.tests.admin.client.ClientProtocolValidationTest
status=CRASH
rc=-1
seconds=4.985
```

Both log files were empty when inspected after the run.

## Investigation

### Harness ruled out

`run-keycloak-suite.ps1`'s `New-ProcessRecord`/`Drain-Running` pattern starts
`StandardOutput.ReadToEndAsync()`/`StandardError.ReadToEndAsync()`
immediately after `Process.Start()`, so both pipes are continuously drained
from the moment the child starts — there is no pipe-buffer-fill deadlock
possible. The `-TimeoutSec` kill path records its exit code as the literal
string `'TIMEOUT'`, never a number, and both crashing classes ran (6.69s,
4.985s) far under the 600s timeout used in the repro — so `rc=-1` came
directly from `System.Diagnostics.Process.ExitCode` after the child exited
**on its own**, not from a harness-issued kill.

### No internal CratonVM path produces this signature

An exhaustive sweep of `vm-cli`, `vm`, `jit`, `classloading`, `native-builtins`
found no code path that reaches `std::process::exit`/`abort`/`ExitProcess`
with a literal `-1`, and — more importantly — **every internal termination
path logs at least one diagnostic line to stderr before the process dies**:

- The top-level `main()` (`vm-cli/src/main.rs:3585-3601`) always
  `eprintln!`s the `Ok`/`Err` result of `run()` (with an explicit flush)
  before calling `std::process::exit(1)`.
- The "visibility-first" panic hook (`vm-cli/src/main.rs:3459-3554`, added in
  an earlier Keycloak diagnosability pass) writes directly to
  `stderr().lock()` and flushes explicitly, bypassing the tracing subscriber
  entirely so it can't be silenced by a WARN+ filter.
- The Windows vectored-exception handler
  (`../../../../vm/src/runtime/crash_handler.rs`, installed via
  `install_hardware_fault_handler()` at `vm-cli/src/main.rs:3387`) writes an
  `hs_err_pid<pid>.log` plus a stderr report for hardware faults, including
  `EXCEPTION_STACK_OVERFLOW` (it skips the stack-walk for that code, since
  walking an exhausted stack would re-fault, but still emits the report),
  then returns `EXCEPTION_CONTINUE_SEARCH` so Windows finishes termination
  normally.
- `System.exit`/`Runtime.exit` (`../../../../native-builtins/src/lang_system.rs`) always
  print `"[cratonvm] System.exit(<code>) called"` first, including when the
  Java code passes `-1` as the exit code.
- `panic = "unwind"` is pinned workspace-wide (not `"abort"`), so a panic on
  any background thread is caught at that thread's boundary rather than
  aborting the whole process silently.

Given this, a *reproducible* CratonVM-internal defect would be expected to
leave at least one line of stderr, not zero.

### Not reproducible on current dev

Direct repro attempts (bypassing the harness, invoking `KcRunner
<class>` on `cratonvm.exe` directly) were run against both classes:

- Individually, on the current classpath (a separate, still-open
  `quarkus-core` missing-dependency gap in `kc-universal-cp.txt` — Keycloak's
  new JUnit 5 test-framework `Config.initConfig()` needs
  `io/quarkus/runtime/configuration/*`, which is entirely absent from the
  classpath): clean `rc=1` with a full `linkage error: no class def found:
  org/keycloak/testframework/config/Config` message both times.
- Concurrently (`-Parallel 2`, matching the original repro) on the current
  classpath: same clean `rc=1` + full message for both.
- Concurrently, with the classpath reconstructed to match the *original*
  broken state (mixed JUnit 5.10.3/6.0.3, reproducing the
  `NamespaceAwareStore.computeIfAbsent` `NoSuchMethodError` that was live at
  the time this bug was recorded): still clean `rc=1` + full message for
  both, matching the other 338 classes in that bucket, not the silent
  signature.

The silent `rc=-1`/empty-output signature did not reproduce under any of
these conditions on the current `dev` build.

### Working theory

No internal code path matches the observed signature, but the OS-level
exit-code convention lines up with an **external** kill:
`TerminateProcess(handle, -1)` (or `(UINT)-1`) is what several common
process-management APIs use by default (e.g. .NET's own
`Process.Kill()` on Windows) when forcibly terminating a process rather
than letting it exit on its own — as opposed to an unhandled Windows
structured exception, which surfaces as its NTSTATUS value (e.g.
`0xC0000005`), not a bare `-1`. Combined with the fact that these two rows
came from a batch run with several `cratonvm.exe` processes running
concurrently (`-Parallel 2` within this mode, plus sibling
`-AllModes` categories/JIT variants potentially running at the same time),
the most plausible external triggers are:

- OS/AV intervention against the JIT's runtime-generated executable code
  (heuristic detection of JIT-compiled machine pages is a well-known
  antivirus false-positive trigger), or
- resource pressure under heavy concurrent `--Xmx 2g` process counts.

Neither can be confirmed post-hoc — the original process is gone and no
crash dump or AV log was captured at the time.

### Applied hardening

Two concrete, low-risk changes were made even without a confirmed root
cause:

1. **`../../../../jit/src/tiered.rs`** — the background JIT compiler thread
   (`cratonvm-jit-compiler`) is now spawned with a 16 MiB stack instead of
   Rust's ~2 MiB default, matching the precedent already established for
   other native-recursion-heavy worker threads (`libcratonvm`'s
   foreign-attach threads) and the main-vm interpreter thread (bumped to
   128 MiB after `binaryTrees(18)`-style recursion overflowed 64 MiB — see
   `../../../../vm-cli/src/main.rs`). This closes the one concretely-identified
   asymmetry: every other stack-hungry worker thread in the codebase had
   already been hardened this way except the JIT compiler thread.
2. **`../../../../apps/keycloak-suite-runner/run-keycloak-suite.ps1`** — a process that
   exits on its own (not a `-TimeoutSec` kill) with a non-zero code AND
   completely empty stdout+stderr is now:
   - retried once automatically, transparently, before anything is
     recorded (`Test-SilentAbnormalExit` + the retry branch in
     `Drain-Running`) — since no internal path produces this signature,
     the working theory is transient external interference, which a retry
     is the correct mitigation for;
   - if the same signature recurs on the retry, recorded as a distinct
     `SILENTEXIT` status (not lumped into generic `CRASH`) with a note
     pointing at this doc, so a future occurrence is immediately
     diagnosable instead of looking like an ordinary linkage-error crash.

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File C:\craton\CratonVM\apps\keycloak-suite-runner\run-keycloak-suite.ps1 `
  -ClassList <tsv with the two classes above> `
  -Category others -Vm craton -Jit on -Start 1 -Count 0 -Parallel 2 -TimeoutSec 600 `
  -RunName <name> `
  -KeycloakRoot C:\craton\CratonVM\apps\keycloak `
  -WorkDir <workdir> `
  -Exe <cratonvm.exe>
```

## Next Steps (if `SILENTEXIT` ever recurs)

- Capture a Windows crash dump (`procdump -ma`) and check Windows Defender /
  AV logs for the PID at the time of the silent exit.
- Check the `SILENTEXIT` row's `retries` count in the note — if it recurred
  even after one retry, that's a much stronger signal of a systemic (not
  transient) issue and this doc should be reopened in `../../../known-issues`.
