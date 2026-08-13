# JIT method statistics on controlled VM exit — fixed

Status: fixed
Found: 2026-07-26 architecture probe
Fixed: `codex/complete-architecture-remediation-20260726`

## Reproduction

Run a finite Java main method on the default real-JDK build:

```bash
CRATONVM_DBG_JIT_METHOD_STATS=1 \
  cratonvm --java-home "$JAVA_HOME" -cp classes Main
```

Observed stderr:

```text
[cratonvm] 1 per-flag variable(s) set directly; the supported spelling is now:
CRATONVM_DBG=jit-method-stats
[cratonvm] main-vm run() returned Ok — VM main exiting normally
```

No JIT method-statistics line is emitted. Repeating with the advertised
`CRATONVM_DBG=jit-method-stats` spelling also emits no statistics.

## Root cause

`jit::tiered::dump_method_stats_to_stderr()` is called only from the
`native_system_exit`/`native_runtime_exit` pre-exit hook installed in
`vm-cli/src/main.rs`. A Java main method that returns normally follows the
launcher's `run() returned Ok` path and never invokes that hook.

The hook also checks `std::env::var("CRATONVM_DBG_JIT_METHOD_STATS")`
directly, while `types/src/flag_groups.rs` advertises
`CRATONVM_DBG=jit-method-stats` as the supported spelling. The grouped spelling
does not satisfy the launcher's direct legacy-variable check.

## Impact

The diagnostic intended to distinguish interpreted, compiled, stuck, and
fallback-dominated workloads is silent on the most ordinary process-exit path.
This blocks evidence-based JIT triage and demonstrates why distributed direct
environment reads cannot be kept consistent with the typed/grouped flag layer.

## Resolution

`JitFlags::method_stats` is now populated from the same resolved `VmFlags`
snapshot as other shared JIT settings. Both the legacy spelling and
`CRATONVM_DBG=jit-method-stats` therefore reach one typed value.

The launcher calls `maybe_dump_jit_method_stats` from the native
`System.exit` pre-exit hook and after an ordinary `run()` return. Its atomic
claim makes the operation idempotent if shutdown paths converge later.

## Verification

`tools/architecture-probe-20260726/check-jit-method-stats-20260726.sh`
compiles a hot finite Java program and runs it twice: once returning normally
and once calling `System.exit(0)`. On the fresh uniquely named release binary,
both paths emitted exactly one statistics record using only the grouped flag:

```text
return: exactly one JIT method-statistics record
exit: exactly one JIT method-statistics record
```
