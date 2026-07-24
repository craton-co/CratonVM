# Cooperative JIT Safepoint Polls

Status: enabled by default. Set `CRATONVM_JIT_SAFEPOINT_POLLS=0` only for
diagnostic comparison.

## Contract

Both x86-64 JIT backends emit a cooperative stop-the-world poll:

- after the method prologue has homed incoming parameters;
- before each loop backedge, including unary, integer, reference, null,
  `goto`, `tableswitch`, and `lookupswitch` shapes.

The fast path is:

```asm
mov  r11, safepoint_flag_addr
test byte ptr [r11], 0xff
jz   clear
call safepoint_slow_path
clear:
```

The single-pass backend flushes register-resident Java values before the
call and records the safepoint map at the call's return PC. The IR backend
already keeps node values in canonical frame slots; methods without complete
typed maps continue to use the conservative frame fallback and therefore
cannot authorize moving-young collection.

`safepoint_slow_path` takes no argument. It resolves the process VM and
current `JvmThread` through the published VM handle and JIT TLS, then enters
the interpreter's existing `safepoint_check`. This makes the same sequence
valid for pure methods and context methods; compiled entry ABIs do not gain
another hidden parameter.

## Memory ordering and publication

`GcBarrier::request_stop_the_world` stores `true` to `stw_requested` with
release ordering. The inline byte load is intentionally non-atomic. A stale
false read only delays arrival until the next bounded poll; after the slow
path observes the request, `safepoint_check` retires the TLAB, drains SATB,
publishes the root snapshot, and arrives at the GC barrier. Barrier
synchronization establishes visibility before the collector scans or moves
objects.

The baked flag address is stable because `SharedVm` is allocated once behind
an `Arc` for the VM lifetime. A zero flag/helper address disables emission in
standalone JIT tests and embeddings that do not publish a VM.

## Coverage and fallback

Cooperative arrival is the normal path. Native OS suspension and
conservative register scanning remain a bounded fallback for a thread
blocked in foreign/native code or for an artifact without complete precise
root metadata. The moving collector must still require
`moving_young_coverage_complete` for every active compiled frame; successful
cooperation by itself is not permission to relocate ambiguous roots.

The prologue poll uses bytecode id `u32::MAX`, avoiding collision with a real
bci-zero safepoint in the precise-map lookup. Backedge polls execute before
the branch condition is materialized, so the slow call cannot invalidate
condition flags.

## Validation

Unit probes execute a pure single-pass method, a pure IR method, and a loop
whose only backedge is conditional while the request byte is set. Runtime
acceptance additionally drives repeated collections while a peer remains in
compiled pure code, with precise-map verification enabled.
