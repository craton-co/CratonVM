# `-agentlib:jdwp=...,includevirtualthreads=y` was parsed and dropped

**Status: fixed behind `CRATONVM_JDWP_AGENT_INCLUDE_VIRTUAL_THREADS` (default ON, `=0` drops the
option as before). Not yet compiled or run.** Filed 2026-12-29 by interpreter round i6 wave 130,
lane L1b. Once the orchestrator has verified it, move this page to `docs/internal/`.

## Symptom

`vm-cli`'s `parse_jdwp_agent_options` accepted `includevirtualthreads=y|n` (a syntax error for
any other value, as the back end's `get_boolean`) and then threw the value away:
`JdwpAgentConfig` had no field for it. So under `-agentlib:jdwp=...,includevirtualthreads=y`,
`VirtualMachine.AllThreads` still left every virtual thread out, where HotSpot's back end
(`gdata->includeVThreads`) lists them. Only the process-environment switch
`CRATONVM_JDWP_INCLUDE_VIRTUAL_THREADS=1` (round i1 wave 49) turned the listing on.

HotSpot 25.0.3, MEASURED (`tools/probes/interp/L1/L1W130JdwpIncludeVirtualThreads.java`, default
flags and `-Xint`, Windows):

| debuggee option | `vt-before` (started before the attach) | `vt-after` (started after) |
|---|---|---|
| none | not listed | not listed |
| `includevirtualthreads=y` | listed, `isVirtual` | listed, `isVirtual` |

`main` is listed in both arms. A virtual thread started before the debugger attached is listed
too: the back end tracks virtual threads from `VMInit` when the option is on, not from the
attach.

## The fix

- `JdwpAgentConfig::include_virtual_threads` (`vm/src/config.rs`), set by
  `parse_jdwp_agent_options` (`vm-cli/src/main.rs`). Per VM: it travels in `VmConfig::jdwp_agent`.
  The embedding entry points build no `JdwpAgentConfig`, so nothing changes for them.
- `debug::start_jdwp_agent` (`vm/src/debug/mod.rs`) sets this VM's
  `DebugState::include_virtual_threads` when the option is `y`, before the server thread starts.
  That is the field `commands::listed_thread_ids` already reads for `AllThreads`.
  `ThreadGroupReference.Children` still lists platform threads only (wave 102: JVMTI
  `GetThreadGroupChildren` has no virtual threads), as on HotSpot.
- `n` (or no option) leaves the field as `CRATONVM_JDWP_INCLUDE_VIRTUAL_THREADS` set it, so the
  environment switch keeps working for the `--jdwp-port` server.
- `CRATONVM_JDWP_AGENT_INCLUDE_VIRTUAL_THREADS=0`: the option is parsed and dropped, as before.

Unit test: `vm-cli`'s `jdwp_agent_options_parse_as_the_back_end_parses_them` now checks that
`includevirtualthreads=y` reaches the config.

## What is not checked by reading

The listing still depends on the VM knowing the thread is virtual (`DebugState::virtual_threads`,
filled at a virtual thread's `ThreadStart` hook) and on the server knowing the thread at all
(`known_thread_ids`: the refreshed thread names and the id manager). Arm B's `vt-before` row
checks the first for a thread that started while no debugger was attached; arm A's checks that a
virtual thread is not listed by mistake when the VM missed its start. If arm B lists `vt-before`
as `true` without `(virtual)`, or arm A lists it at all, the gap is in that bookkeeping, not in
this option.

## Verify

    javac -g -d out tools/probes/interp/L1/L1W130JdwpIncludeVirtualThreads.java
    java -cp out L1W130JdwpIncludeVirtualThreads                      # plain: "ended"
    # arm A
    cratonvm [--compatible] --java-home $JDK -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5895 -cp out L1W130JdwpIncludeVirtualThreads wait &
    java -cp out L1W130JdwpIncludeVirtualThreads debug 5895
    # arm B
    cratonvm [--compatible] --java-home $JDK -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5895,includevirtualthreads=y -cp out L1W130JdwpIncludeVirtualThreads wait &
    java -cp out L1W130JdwpIncludeVirtualThreads debug 5895

Pass: arm A's debugger stdout equals the `HOTSPOT_EXPECTED` block, arm B's the
`HOTSPOT_EXPECTED_B` block, in both modes (debug build, `experimental-debug`). The debuggee
prints `ended`. Negative control: arm B with `CRATONVM_JDWP_AGENT_INCLUDE_VIRTUAL_THREADS=0`
prints arm A's block.

    cargo test -j 5 -p cratonvm-cli --bin cratonvm jdwp_agent_options_parse_as_the_back_end_parses_them

## Wave 132 (lane L9 review), 2026-10-19

The parse, the per-VM `DebugState::include_virtual_threads` set before the
server starts, and the `=0` escape are correct. All three `JdwpAgentConfig`
literals (in `vm-cli` only) carry the new field. Registration is complete:
`flag_groups.rs` and `flag-surface.txt`. Fixed in place: a missing space in
`let server_ended = Arc::new(...)` (`vm/src/debug/mod.rs`, a rustfmt diff).
Status unchanged.
