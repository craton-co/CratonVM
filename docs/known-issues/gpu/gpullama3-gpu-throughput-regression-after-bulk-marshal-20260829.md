# GPULlama3 GPU throughput regressed ~42% after the bulk-marshal / GEMM-tile GPU changes landed

## Status
New finding, 2026-08-29. Reproduced 5x in isolation, no host contention. Not
yet bisected to a specific commit or root-caused to a mechanism. Correctness
is unaffected — same correct output every run, before and after.

## The regression

Same host, same RTX 2060, same command (`run-craton-gpu.sh`, `--gpu`,
`Llama-3.2-1B-Instruct-F16.gguf`, greedy decode, prompt `"Why is the sky
blue?"`, 32 tokens), only the binary changed:

| build | commit range | achieved tok/s |
|---|---|---|
| `target-gpu`, built 08:22 | dev tip before `8cf39edee` | 20.24, 21.35, 20.26 (avg **20.6**) |
| `target-gpu`, rebuilt 09:43 | dev tip after `8cf39edee`/`d51ae1eab`/`c9a7a2b13` (bulk-marshal + GEMM 8x8 tile) | 11.32, 12.33, 11.37, 12.23, 12.16 (avg **11.9**) |

**~42% slower.** Output stayed byte-identical across every run on both
builds: `"The color of the sky is blue because of a phenomenon called
Rayleigh scattering,"`. This is a pure throughput regression, not a
correctness one.

## What landed between the two builds

```
8cf39edee perf(gpu): bulk-copy array payloads instead of one element at a time
d51ae1eab perf(gpu): add an 8x8 tile and select it by output size
c9a7a2b13 perf(gpu): register-block the GEMM kernels, and vectorise the shared reads
75697269f feat(gpu): GEMM transpose via strides, and caller-chosen streams
d537c4044 feat(gpu): built-in GEMM kernels, fp32 and fp16
```

merged into dev at 09:12:35 -03:00 (branches `perf/tohost-bulk-marshal-20260829`
and `perf/gemm-8x8-tile-20260829`).

## Why this is plausible, not yet confirmed

These commits target exactly the two mechanisms GPULlama3's forward pass on
CratonVM's GPU path depends on: bulk H2D/D2H array marshalling and GEMM
kernel dispatch (the transformer's per-layer matmuls). The ray tracer kernel
bench, run on the same two builds, moved the *other* direction — a small
**improvement** (1.0849ms -> 1.0607ms at 1920x1440, ~2.2%) — but the ray
tracer does one dispatch of one big flat output array per frame, which is
exactly the shape "bulk-copy instead of one element at a time" targets. If
GPULlama3's per-layer/per-token dispatch pattern is structured differently
(many smaller GEMM calls across 16 transformer layers per token, versus one
big array out), a change tuned for the bulk-single-array case could plausibly
regress a many-small-calls case — but this is a hypothesis from the shape of
the two workloads, not measured or profiled here.

## Not yet done
- Bisect across the 5 commits above individually to find which one owns the
  regression (or whether it's their combination).
- Profile a single forward pass (`CRATONVM_GPU_TRACE_BYTES=1` and/or
  `CRATONVM_GPU_DUMP_PTX`) on both builds to see what actually changed in the
  dispatch shape for this model's matmul sizes.
- Confirm whether `--gpu-min-work`/GEMM tile selection is choosing a
  different (worse) tile size for GPULlama3's specific matrix dimensions
  than it did before `d51ae1eab` added tile selection.
- No HotSpot-side comparison needed here — HotSpot doesn't run this GPU path
  at all; the regression is CratonVM-GPU-build-over-CratonVM-GPU-build only.

## Repro

```bash
cd apps/GPULlama3.java
bash run-craton-gpu.sh <cratonvm.exe> -p "Why is the sky blue?" -n 32
# compare a binary built before 8cf39edee against one built after
```
