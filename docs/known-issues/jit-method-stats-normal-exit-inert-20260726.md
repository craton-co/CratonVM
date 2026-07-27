# JIT method statistics are not emitted on normal VM exit

Status: open
Found: 2026-07-26 architecture probe
Base: `dev` at `3be41785e`

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

## Required fix

1. Read one typed flag value, not the legacy environment variable.
2. Put an idempotent diagnostic flush in a common shutdown guard used by
   normal return, `System.exit`, launcher error, and controlled fatal exit.
3. Add subprocess tests for normal return and `System.exit(0)` using only the
   supported grouped spelling. Each must emit exactly one statistics record.
