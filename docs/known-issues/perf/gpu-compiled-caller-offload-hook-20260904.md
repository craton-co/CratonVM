# A compiled caller can offload — but not while it also writes arrays

**Status:** built, measured, and **opt-in**
(`CRATONVM_GPU_JIT_GATE_CALLERS=hook`). The default is unchanged.
**Blocker:** `bench-gpu/runtime-stress.sh`'s `cache_coherence` scenario
fails under it. Not root-caused.

## The residual this was for

`runtime::offload_jit_gate` refuses JIT admission to a method containing
an `invokestatic` whose target is a GPU kernel the dispatcher can launch.
That refusal is what keeps the interpreter's offload hook able to see the
site — compiled code has no such hook, so a compiled caller stops
offloading permanently.

Everything else about that gate was narrowed on 2026-09-04
(`fix/gpu-jit-gate-overbroad-20260904`). This one reason survived,
described there as "a trade rather than a bug".

## What it costs, measured rather than asserted

`GpuHookOverheadBench` under `--gpu`, one binary,
`CRATONVM_GPU_JIT_GATE_CALLERS` the only difference between arms.
`base_ns_per_call` is a loop calling an **ineligible** target, so the
offload hook is not in it at all — the only thing that arm measures is
the enclosing method being denied compilation:

| arm | `base_ns_per_call` |
|---|---:|
| `block` (the default) | 407.9 |
| `0` (compile, no hook) | 10.2 |
| no `--gpu` | 9.0 |

**40x, charged to every line of the method**, not to the kernel call it
was refused for. That is the prize, and it is why this was worth
building rather than tuning the refusal with a give-up-after-N counter.

## What was built

`jit::offload_hook` — a registry of kernel targets. `offload_jit_gate`
already resolves every `invokestatic` in a method it judges and decides
whether the target is one the DISPATCHER can launch; under `hook` it
records the target there instead of refusing the caller.

Then the compiler keeps those sites reachable by the hook, and the hook
runs from compiled code:

* **Three doors** bypass `jit_invoke_dispatch` for a statically-bound
  call, and all three are closed for a registered target:
  the IR tier's `direct_calls` map, the single-pass **inliner**
  (`inline_sites`, checked first — "most profitable"), and the
  single-pass `direct_calls_idx`. The inliner was the one that mattered:
  with only the IR door closed, `main` still spliced the kernel body in
  and the census read zero.
* **`jit_invoke_dispatch`** decodes the argument slots against the
  descriptor and calls `offload::try_dispatch`. `frame_idx` reaches only
  a `tracing::debug!`, which is compiled out of release, so there is no
  compiled frame index to invent.
* **Per-site state**, keyed by `JitInvokeInfo` address — which IS one
  call site, so nothing here can deoptimise a sibling. It holds both the
  "is this a kernel" answer and the decline streak, so a site that has
  declined `CRATONVM_GPU_MIN_WORK_GIVEUP` times stops consulting the
  hook.

### It works

`considered=9519 offloaded=9005 declined=514 bailed=0 sites_retired=2` —
9,005 kernel launches from a compiled caller, which was impossible
before. All three bench scenarios beat the default:

| | `base` | `small` | `big` |
|---|---:|---:|---:|
| `block` | 358.5 | 13534.8 | 127257.4 |
| `hook` | 13.2 | 358.5 | 79983.0 |
| no `--gpu` | 15.4 | 28.5 | 2030.7 |

## Why it is not the default

`bench-gpu/runtime-stress.sh`:

```
FAIL cache_coherence: control=-6705490297358015087 gpu=-8633255346386885231
```

Every other scenario passes, and the same suite passes on every other
arm. What `cacheCoherence` does that the others do not is host-write its
input array **inline, in the same method** as the `scale(in, out)` call
— so under `hook` that one method both writes a primitive array from
compiled code and offloads from compiled code.

Two switches localise it, and neither is the retirement policy:

| arm | `cache_coherence` |
|---|---|
| `hook` | **FAIL** |
| `hook` + `CRATONVM_GPU_JIT_ARRAY_WRITERS=refuse` | PASS |
| `hook` + `CRATONVM_GPU_MIN_WORK_GIVEUP=0` | FAIL |
| `block` (default) | PASS |

`ARRAY_WRITERS=refuse` keeps array writers interpreted, so their stores
call `input_cache::invalidate` directly instead of going through the
compiled-tier barrier's deferred dirty mark. That it passes says the
fault is in how the barrier's deferral composes with an offload issued
from the same compiled method — not in the argument decode, not in the
door closing, and not in the give-up policy.

That is a hypothesis with two supporting arms, not a diagnosis. The
barrier itself is sound in isolation: `jit-writer-stale.sh` fires it
5,497 times against a live cache with checksums bit-identical to
HotSpot.

## Where to start

The drain runs at the top of every `input_cache` getter and at the top
of `remap_and_sweep`. The compiled offload path reaches the getters
through `dispatch_method_sync`, the same as the interpreted one, so the
ordering *looks* right — which is exactly why this needs a trace of
mark-vs-drain-vs-marshal on one `cacheCoherence` round rather than more
reading.

`n=65536`, 12 rounds, and the divergence is stable run to run, so a
per-round dump of `(dirty buckets, filter word, whether the marshal hit
the cache)` will name the round where the device copy stops matching the
heap.
