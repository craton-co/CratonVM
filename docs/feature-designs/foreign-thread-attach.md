# Foreign-thread attach and safepoints

`AttachCurrentThread` and `AttachCurrentThreadAsDaemon` register a host OS thread
as a `JvmThread`; `DetachCurrentThread` reclaims its VM state. The implementation
is in `vm/src/native/jni.rs` and the embedding tests in `libcratonvm`.

An attached thread publishes its TLAB and thread address to the thread registry.
It is a counted mutator during a Java call and GC-blocked while idle between
calls. It participates in stop-the-world root publication, and detach refuses
a thread with a call still in flight. `CRATONVM_FOREIGN_ATTACH=0` disables this
integration and restores the legacy environment-only behavior.

Resolution still uses the process-global `PROCESS_VM` cell: attach does not
select a VM from the supplied `JavaVM*`. This limits multi-VM embedding.
See the [embedding contract](embedding-api.md) and
[per-VM state](../architecture/per-vm-state.md).
