# JIT caller gate for automatic GPU offload

Automatic (`--gpu`) offload is initiated from the interpreter's `invokestatic`
path.
If a caller were JIT-compiled or OSR-compiled, it could bypass that hook and
silently execute the callee on CPU instead.

With `--gpu` enabled, `vm/src/runtime/offload_jit_gate.rs` identifies callers
containing offload-eligible `invokestatic` sites and keeps those callers out of
JIT and OSR compilation. The gate is wired through all interpreter admission
checks, so the invocation continues through the offload dispatcher for the
lifetime of the eligible call site.

This is a correctness and observability guard, not a permanent general JIT
restriction: callers without an eligible offload site are unaffected.

See also the historical validation record.
