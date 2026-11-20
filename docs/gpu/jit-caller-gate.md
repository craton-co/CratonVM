# JIT caller gate and compiled-caller hook for automatic GPU offload

Automatic (`--gpu`) offload enters through both the interpreter's
`invokestatic` path and the compiled-tier dispatch helper.

Callers containing offload-eligible `invokestatic` sites are allowed to compile in
both the single-pass and optimizing (IR) tiers rather than being pinned to the
interpreter. When `--gpu` is active, `vm/src/runtime/offload_jit_gate.rs`
registers offload-eligible targets (and arms the hook for forward-referenced
classes) in `cratonvm_jit::offload_hook`, which `OffloadCacheRegistry` evicts
per VM on teardown.

Across direct-call planning, single-pass inlining, and optimizing-tier body
splicing (`vm/src/runtime/interpreter/jit_bridge.rs::resolve_inline_site_from_body`
and `jit/src/lib.rs::append_ir_inline_site`), `keeps_dispatch_helper` refuses
static direct binding and bytecode inlining for GPU-candidate targets so those
call sites continue routing through `jit_invoke_dispatch` and remain visible to
compiled-caller offload. Callers without GPU-candidate sites pay only a single
relaxed boolean check at compile time and one per-dispatch relaxed check when
GPU offload is unarmed.

See also the [GPU offload reference](README.md) and [hardware CI gates](ci.md).
