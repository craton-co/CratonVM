# Rust ports of the C agent shims

Every C JVMTI probe of `tools/probes/interp/L1/` is a Java class plus a C
shim, loaded as `-agentpath` and with `System.load`. The Windows
development box has no C compiler, so the expected HotSpot lines of a probe
are measured with a Rust port of its shim: a `cdylib` that calls the JNI and
JVMTI function tables by slot number and prints exactly the C shim's lines.
The orchestrator's Linux host runs the C shim; a difference between the two
on HotSpot is a port bug, not a VM one.

Proposal (built in interpreter round i2 wave 53; an internal record):
`interpreter-L1-proposal-rust-ports-of-the-c-agent-shims-measure-hotspot-locally-FIXED-20261017.md`.

| port | C shim |
|---|---|
| `L1W52JvmtiThreadAndClassFacts` | `tools/probes/interp/L1/L1W52JvmtiThreadAndClassFacts.c` |
| `L1W52JvmtiExceptionCatch` | `tools/probes/interp/L1/L1W52JvmtiExceptionCatch.c` |
| `L1W53JvmtiClassHooks` | `tools/probes/interp/L1/L1W53JvmtiClassHooks.c` (also the shim of `L1W54JvmtiAgentInitError`, with its `fail` option) |
| `L1W54JvmtiRetransform` | `tools/probes/interp/L1/L1W54JvmtiRetransform.c` |
| `L1W55JvmtiPhases` | `tools/probes/interp/L1/L1W55JvmtiPhases.c` |
| `L1W55JvmtiLambdaClass` | `tools/probes/interp/L1/L1W55JvmtiLambdaClass.c` |
| `L1W55AttachNativeAgent` | `tools/probes/interp/L1/L1W55AttachNativeAgent.c` |
| `L1W56JvmtiLambdaMethods` | `tools/probes/interp/L1/L1W56JvmtiLambdaMethods.c` |
| `L1W56JvmtiReentryContended` | `tools/probes/interp/L1/L1W56JvmtiReentryContended.c` |
| `L1W56JvmtiThreadTimes` | `tools/probes/interp/L1/L1W56JvmtiThreadTimes.c` |
| `L1W56JvmtiPrimitiveClass` | `tools/probes/interp/L1/L1W56JvmtiPrimitiveClass.c` |
| `L1W57JvmtiWaitStates` | `tools/probes/interp/L1/L1W57JvmtiWaitStates.c` |
| `L1W57JvmtiLocalVariableTable` | `tools/probes/interp/L1/L1W57JvmtiLocalVariableTable.c` |
| `L1W57JvmtiRedefine` | `tools/probes/interp/L1/L1W57JvmtiRedefine.c` |
| `L1W57JvmtiErrorNames` | `tools/probes/interp/L1/L1W57JvmtiErrorNames.c` |
| `L1W58JvmtiInNative` | `tools/probes/interp/L1/L1W58JvmtiInNative.c` |
| `L1W58JvmtiVirtualCpuTime` | `tools/probes/interp/L1/L1W58JvmtiVirtualCpuTime.c` |
| `L1W60JvmtiLambdaFrames` | `tools/probes/interp/L1/L1W60JvmtiLambdaFrames.c` |
| `L1W60JvmtiLambdaMethodEvents` | `tools/probes/interp/L1/L1W60JvmtiLambdaMethodEvents.c` |
| `L1W61JvmtiAgentStartup` | `tools/probes/interp/L1/L1W61JvmtiAgentStartup.c` |
| `L1W61JvmtiFrameControl` | `tools/probes/interp/L1/L1W61JvmtiFrameControl.c` |
| `L1W61JvmtiEventAndBreakpointErrors` | `tools/probes/interp/L1/L1W61JvmtiEventAndBreakpointErrors.c` |
| `L1W61JvmtiStepIntoIntrinsic` | `tools/probes/interp/L1/L1W61JvmtiStepIntoIntrinsic.c` |
| `L1W62JvmtiCallbackFrameControl` | `tools/probes/interp/L1/L1W62JvmtiCallbackFrameControl.c` |
| `L1W63JvmtiAgentFunctions` | `tools/probes/interp/L1/L1W63JvmtiAgentFunctions.c` |
| `I41ObsoleteLockingLoops` | none: written as a port (the shim of `tools/probes/interp/L2/I41ObsoleteLockingLoops.java`) |
| `L1W63JvmtiTagsAndHeap` | `tools/probes/interp/L1/L1W63JvmtiTagsAndHeap.c` |
| `L1W63JvmtiEvents` | `tools/probes/interp/L1/L1W63JvmtiEvents.c` |
| `L1W63JvmtiEarlyHooks` | `tools/probes/interp/L1/L1W63JvmtiEarlyHooks.c` |
| `L1K54JvmtiLambdaProxyLocalsAndFields` | `tools/probes/interp/L1/L1K54JvmtiLambdaProxyLocalsAndFields.c` |
| `L1K54JvmtiLambdaProxyEvents` | `tools/probes/interp/L1/L1K54JvmtiLambdaProxyEvents.c` |
| `L1K54JvmtiLateAgentLambdaFrames` | `tools/probes/interp/L1/L1K54JvmtiLateAgentLambdaFrames.c` |
| `L1K54JvmtiLambdaProxyConstantPool` | `tools/probes/interp/L1/L1K54JvmtiLambdaProxyConstantPool.c` |
| `L6K54BlockedSyncNativeAndEntryOrder` | `tools/probes/interp/L6/L6K54BlockedSyncNativeAndEntryOrder.c` (a lane-L6 probe; its port lives here with the common helpers) |

## Layout

Each port is its own Cargo workspace (`[workspace]` in its `Cargo.toml`), with
no dependencies, so it is never a member of the VM's workspace and never
part of its build. `common/agent.rs` holds the slot-call macros (`jvmti!`
by the 1-based number `jvmti.h` gives a function, `jni!` by the 0-based index
in `JNINativeInterface_`), `GetEnv`, `Deallocate`, `GetClassSignature` and
`SetEventCallbacks`; a port includes it with `#[path]` rather than as a
crate. The two wave-52 ports predate it and are standalone.

The ports are outside every crate, but several tree-wide source gates walk
every `.rs` file under the workspace root (`types/tests/string_literal_line_breaks.rs`,
`types/tests/flag_declaration_guard.rs`, `types/tests/flag_env_mutation_guard.rs`,
`vm-cli/tests/no_diag_eprintln.rs`). A port therefore keeps to their rules: no
string literal with a run of spaces or a raw line break, no `CRATONVM_*`
literal, no `set_var`, no diagnostic-tag `eprintln!`. The gates that count
`static`s, panics or test-only APIs read named crates' `src` only.

## Run (PowerShell)

```text
$env:CARGO_TARGET_DIR = 'C:\craton\tgt-ports'
cargo build --release --manifest-path tools/probes/interp/L1/rust-ports/L1W53JvmtiClassHooks/Cargo.toml
javac -d $env:TEMP\l1w53 tools/probes/interp/L1/L1W53JvmtiClassHooks.java
$dll = 'C:\craton\tgt-ports\release\l1w53_jvmti_class_hooks.dll'
java "-agentpath:$dll" -cp $env:TEMP\l1w53 L1W53JvmtiClassHooks $dll
```

The library name is the package name with `-` as `_`. Under CratonVM the same
two lines run with `cratonvm` in place of `java`.

## Adding a port

1. Copy `L1W53JvmtiClassHooks/` and rename the package.
2. Port the C shim call by call; print with the same format strings.
3. Run it on HotSpot three times; paste the lines into the probe's header as
   MEASURED, naming the port.
