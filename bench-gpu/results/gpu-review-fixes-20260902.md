# GPU crate review fixes — hardware validation, 2026-09-02

Branch `fix/gpu-review-four-20260902`, RTX 2060 (sm_75), CUDA 13.3, Windows 11,
release `gpu-driver` build. `NEW` is this branch; `OLD` is the 2026-08-29
`gpu-driver` binary from the main checkout (an older tree — checksum and
coarse timing only, not a same-binary A/B). HotSpot is Adoptium 25.0.3.

The four review items, each shipped with a kill switch:

| item | change | switch |
|---|---|---|
| 1 | completion reaper polls `Event::query`; no `cuLaunchHostFunc` per launch | `CRATONVM_GPU_HOST_CALLBACK=1` restores the callback |
| 2 | `cuda_bridge::critical` registry wired into offload + every collector; bounded drain, relocation veto | (none — correctness) |
| 3 | reductions fold each warp with `shfl.sync.down` before ONE `red.global.add` per warp | (none — codegen; `GATE_REDUCTION=1` in `ci-gate.sh` checks it) |
| 4 | device `AllocPool` (exact-size free list, 512 MiB cap); dead barrier `cuEventRecord` and dead streams removed | `CRATONVM_GPU_DEVICE_POOL=0` |

## Item 3 — reduction (`GpuDotBench 16777216 5`)

| arm | dot_ms | checksum |
|---|---|---|
| HotSpot | 1719 | -58730497593000 |
| OLD `--gpu` | 23 | -58730497593000 |
| NEW `--gpu` | 2 | -58730497593000 |

`ci-gate.sh` with `GATE_REDUCTION=1`: ALL GATES PASSED (GpuWarm, GpuCompute,
BoundsDeopt2, GpuProbe DOT_CHECKSUM all match HotSpot). The 15 ptxas
round-trip tests in `jit-cuda` pass, including `ptxas_round_trip_dot_reduction`.

## Item 1 — async chain (`GpuAsyncChainBench 65536 400 5`, best round)

Same binary, only the switch differs. 400 launches of a 64 Ki-int vecAdd on
resident `GpuArray`s, awaited once.

| arm | best_total_ms | us/launch |
|---|---|---|
| NEW poll (default) | 20.66 | 51.6 |
| NEW `CRATONVM_GPU_HOST_CALLBACK=1` | 24.45 | 61.1 |
| OLD (callback era) | 26.25 | 65.6 |

`SpontaneousCompletionCheck 4194304`: PASS — the reaper completes a
submission with no future call on it.

## Item 4 — transfer floor (`GpuTransferFloor 2764800 30`, best_ms)

Interleaved rounds, same binary, pool on vs `CRATONVM_GPU_DEVICE_POOL=0`:

| round | pool on | pool off |
|---|---|---|
| 1 | 1.227 | 1.288 |
| 2 | 1.662 | 2.040 |
| 3 | 1.415 | 1.473 |
| 4 | 1.410 | 1.683 |
| 5 | 1.507 | 1.257 |

Pool on wins 4 of 5 rounds; best-of-5 1.227 vs 1.257 ms. Checksum
11466174412800 in every run. The pool stays default-on: the gain is small on
this workload (one resident input, one chunked writeback) and the point of
the pool is the allocation-per-dispatch shape, where `cuMemAlloc` +
`cuMemFree` is the floor.

Pinned-host staging for H2D uploads was prototyped behind
`CRATONVM_GPU_PINNED_H2D=1` and measured neutral here (1.41 vs 1.33 ms
best, single runs), and slower (0.77-0.90x) in the independent bandwidth
measurement in `cuda-bridge/tests/transfer_bandwidth_it.rs`; it was removed
rather than left as an opt-in that is never a win.

`GpuWarm f 4194304 5`: warm_ms=2, SAMPLE=-1430079446 (matches HotSpot via
`ci-gate.sh`).

## Item 2 — critical sections

No hardware number: this is a correctness wiring. Coverage is in
`cuda-bridge/src/critical.rs` tests (forbidden holder vetoes relocation on
timeout, keep-alive holder does not delay clearance, forbidden acquisition
waits for the moving cycle), `gc/src/vm_heap.rs`
(`drain_gives_up_after_its_budget_and_vetoes_relocation`) and the
`vm::runtime::offload` tests. Each collector reports its refusal as a
non-moving reason (`nonmoving-gpu-critical-section`,
`g1-no-evacuation-gpu-critical-section`) in the GC metrics report.

## Engagement census

The `[cratonvm] gpu events: ... device allocs: cuMemAlloc=N pooled=M`
line prints at exit for any run that launched a kernel. It used to sit
behind the dispatch-timing `calls == 0` gate, which the `--gpu`
auto-offload path never moves, so none of the benches above printed it;
fixed in this branch (see `dispatch_timing::report`).
