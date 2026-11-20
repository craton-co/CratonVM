# JVMTI delivery and VM attribution

JVMTI is partial and requires the debugging feature configuration. Interpreter
events are delivered synchronously on the raising thread and carry its VM
identity. The owning code is `vm/src/runtime/interpreter/jvmti_events.rs`,
`vm/src/runtime/jvmti.rs`, and `vm/src/jvmti/`.

The C environment table in `vm/src/jvmti/native_env.rs` supports a subset of
capabilities, event callbacks, breakpoint handling, class/method metadata,
allocation, and environment lifecycle operations. `jni_get_env` recognizes
supported JVMTI versions. Unsupported operations return a JVMTI error;
the presence of an environment table is not full agent compatibility.

Events must be attributed to the VM that owns the raising thread and objects.
A process-wide or unattributed fallback must not let an agent observe another
VM's events. Agent loading and initialization must preserve environment lifetime
and callback ordering. See [per-VM state](../architecture/per-vm-state.md) for
remaining shared-state constraints, and the native environment implementation
and its tests for the supported function slots.
