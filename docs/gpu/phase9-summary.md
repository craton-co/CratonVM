# Phase 9 summary — deferred-writeback + honest stop

Phase 8's backlog promised three items for Phase 9. One landed
concretely; the other two are deferred again with a more detailed
rationale for why a partial-attempt would deliver negative value.

## Landed

### Phase 9 #1 — skip mid-pipeline D→H writeback (`fb7be44`)

Phase 7 #2 cached the `DeviceBuffer<T>` across kernels so the
H→D upload only ran once per pipeline. Phase 9 #1 does the same
for the D→H direction:

- New `device_cache::Entry { buf, dirty }` wraps each cached
  buffer; the dirty bit is set by post-kernel writebacks instead
  of an eager download.
- New `device_cache::download_into_bytes_if_dirty(handle)`
  snapshots the matching `Arc<DeviceBuffer<T>>` + clears the
  dirty bit under the mutex, then drops the lock before doing
  the cudarc D→H copy. Returns `None` for clean / unknown
  handles.
- `MarshalWriteback::Resident*::writeback` collapses to one
  line: `device_cache::mark_dirty(handle)`. The four arms keep
  their separate variants for type-erasure clarity, but the
  download work is gone.
- New `NativeContext::gpu_array_download_if_dirty` escape hatch
  (default `None`; VM override delegates to `device_cache`).
- `builtin_array_to_host` calls the escape hatch first; on
  `Some(bytes)` it stamps them into the resident store via
  `array_replace_bytes` BEFORE rebuilding the Java array.

Net effect on a chained pipeline:

  arr = GpuArray.wrap(input);
  for (i = 0; i < N; i++) exec.submit("P", "step" + i, ...).get();
  result = arr.toHost().get();

Before Phase 7 #2: N × (H→D + K + D→H). Before Phase 9 #1:
H→D + N × (K + D→H). After Phase 9 #1: H→D + N × K + D→H.

## Deferred (with more detail than Phase 8's summary)

### Phase 9 #2 — non-static lambda targets (was Phase 8 #2)

What's needed for honest delivery:

1. **Analyzer relaxation**: today `analyze_with_annotations` rejects
   non-static methods at line 143 with `Reason::NonStatic`. Easy
   to lift, but doing so in isolation means the dispatch then
   proceeds to:
2. **PTX emitter**: `bind_param_locals` currently maps method-
   slot N to descriptor-param N, assuming the method is static.
   For a non-static method, slot 0 is the receiver `this` (not
   in the descriptor). The emitter would crash, or — if we leave
   slot 0 unbound — any `aload_0` opcode in the body would
   fail to resolve a register.
3. **Marshaller**: kernel descriptor doesn't include the
   receiver. Need to know whether to skip captures[0] or pass it
   as a ghost first arg.

The realistic minimal scope wires (1) behind an admission hint
(`@GpuKernel(admit = ALLOW_NON_STATIC_TRIVIAL)`) and adds a
body-pre-scan that checks for `aload_0` / `iload_0` etc. before
admitting. The user opts in claiming the method is
essentially-static. (3) follows the descriptor faithfully —
captures map 1:1 to descriptor params, with the receiver
silently passed but not consumed. (2) needs no change because
the body never uses slot 0.

That's roughly 80 LOC across three files. It would land easily.
But the use case is narrow — "static helper methods that happen
to live on an instance" — and the much more common
`obj::method` / `Class::instanceMethod` lambdas (where the body
DOES use `this.field`) still wouldn't work.

For the common case, the work is much bigger: the PTX emitter
needs to handle `getfield` against a device-resident receiver.
That requires either:
- Escape-analysing the receiver's relevant fields at dispatch
  time and threading them as kernel args (deopts the lambda if
  the receiver's class doesn't admit it), or
- Marshalling the receiver as a typed device buffer + emitting
  `ld.global.*` against fixed field offsets per the receiver's
  class layout (needs the receiver's `Class` object on the
  dispatch path).

Both are real JIT-compiler features. The narrow version is
defensible only as scaffolding for the full version. Without a
real-GPU pipeline that demonstrates the broader use case is
needed, shipping scaffolding alone is negative value (adds API
surface that has to be supported but doesn't unblock anything).

Verdict: defer until either (a) a GPU benchmark surfaces a real
non-static-lambda use case that the user wants, or (b) a Phase
10 design that targets the full feature with both layers
landing together.

### Phase 9 #3 — CUDA Graph capture (was Phase 8 #4)

Status unchanged from Phase 8 summary. The largest scope item
in the backlog; needs a real-GPU performance baseline first to
prove the benefit before doing the work. The benchmark scaffold
shipped in Phase 8 #3 is the prerequisite for that baseline.

Verdict: blocked on Phase 8 #3 benchmark results from a GPU
host. Until those numbers exist, the entire Graph-capture
direction is speculative.

## Verification on no-GPU dev box

  cargo check --workspace                                       clean
  cargo check --workspace --features cratonvm-vm/gpu-offload     clean
  cargo test  -p cratonvm-vm --features gpu-offload --lib offload
                                                                6 passed, 3 ignored
  cargo test  -p cratonvm-gc --features gpu-offload --lib        680 passed

## What this means for the project

The host-side GPU offload stack is now "feature-complete enough
that further improvements require real-GPU measurement first".
The remaining items in the backlog are all perf-or-niche-feature
work whose return is unverifiable without hardware.

The single highest-leverage next task remains: run the Phase 8
#3 benchmark scaffold on an NVIDIA box and paste real numbers
into `docs/gpu/first-results.md`. Those numbers unblock:

- Phase 9 #3 (CUDA Graph capture) — needs the chain-pipeline
  baseline to motivate the work.
- Confirms Phase 7 #1 (real async overlap) actually overlaps
  on a GPU. Today the overlap is correct by construction but
  unmeasured.
- Validates Phase 9 #1's deferred-writeback path — should show
  a step-down for the chain-pipeline pattern relative to the
  pre-Phase-9 binary.

Without that data, Phase 9+ optimization work is shooting
blindly. With it, the path forward is clear.
