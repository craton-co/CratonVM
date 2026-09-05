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

## Why it is not the default — and it is not this feature's fault

`bench-gpu/runtime-stress.sh`:

```
FAIL cache_coherence: control=-6705490297358015087 gpu=-8633255346386885231
```

**The first reading of this was wrong and is worth keeping visible.** It
was recorded as "the compiled-tier array barrier's deferred dirty mark
does not compose with a compiled-tier offload", on two arms:
`CRATONVM_GPU_JIT_ARRAY_WRITERS=refuse` passed, and
`CRATONVM_GPU_MIN_WORK_GIVEUP=0` still failed. Both of those are true
and both are consistent with a completely different cause, because both
of them also stop the method being COMPILED.

Three more arms settled it:

| arm | `cache_coherence` |
|---|---|
| `hook` | WRONG |
| `hook` + `ARRAY_WRITERS=allow` (residency cache **off**) | WRONG |
| `hook` + `--gpu-min-work 999999` (**nothing offloads**) | WRONG |
| **`cratonvm`, no `--gpu` at all, JIT on** | **WRONG** |
| `cratonvm --nojit` | right |
| `cratonvm CRATONVM_JIT_OSR=0` | right |

With the cache disabled the answer is still wrong, so nothing is going
stale. With offload disabled entirely it is still wrong, so no kernel is
involved. With no `--gpu` at all it is still wrong, so neither is this
feature.

`GpuRuntimeStress.cacheCoherence` is **miscompiled by OSR**.
`CRATONVM_JIT_DENY` on that one method fixes it; denying `scale`, `sum`
or `mix` does not. See
`docs/known-issues/jit/osr-miscompiles-cachecoherence-20260904.md`.

### What this gate was doing

Hiding that defect. All three of `runtime-stress.sh`'s arms avoid
compiling the method — HotSpot, `--nojit`, and `--gpu`, where
`offload_jit_gate` refuses it for both of its reasons at once. This mode
is the first thing that ever compiled it.

So it stays opt-in, for a reason that is about the OSR defect and not
about the hook: turning it on would expose that miscompilation to every
`--gpu` run. Once the OSR bug is fixed this should become the default —
the 27-42x is real, and nothing in this feature is implicated in the
wrong answer.
