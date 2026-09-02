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

Items 1 and 3 are wins on hardware. Item 2 is correctness with no
timing claim. Item 4 is the honest one: the pool engages on one of the
three workloads measured, and its first version carried a defect of the
same kind as item 1 — see below.

**A caveat on the absolute timings.** The host is a daily-driver
workstation and another session was compiling during part of this
validation (load 90%, three `rustc` processes). Numbers taken then are
marked; the direction of each A/B held in every round, but the absolute
per-launch figures drifted about 2x between a quiet host and a busy
one, so read the arms against each other and not against the clock.

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

Final interleaved run on the merged tree, quiet host (load 60, no other
compile running), four rounds of each arm, us/launch:

| round | poll (default) | `CRATONVM_GPU_HOST_CALLBACK=1` |
|---|---|---|
| 1 | 58.1 | 59.5 |
| 2 | 57.9 | 62.7 |
| 3 | 62.1 | 72.2 |
| 4 | 49.7 | 70.1 |

Poll wins every round; best-of-four 49.7 against 59.5. The 2026-08-29
binary, which had no switch, ran 65.6 on the same bench. Checksum
`8589869056` in every run.

`SpontaneousCompletionCheck 4194304`: PASS — the reaper completes a
submission with no future call on it.

## Item 4 — device allocation pool

**Read the census before the timings.** The exit line
`[cratonvm] gpu events: ... device allocs: cuMemAlloc=N pooled=M parked=P`
says whether the pool was engaged at all, and on two of the three
workloads it was not:

| workload | cuMemAlloc | pooled | parked |
|---|---|---|---|
| `GpuTransferFloor 2764800 30` | 2 | 0 | 0 |
| `GpuAsyncChainBench 65536 400 5` | 22-58 | 0 | 3 |
| `GpuDotBench 16777216 5` | 4 | 5 (55.6%) | 6 |

The transfer floor allocates its buffers once and holds them, so the
earlier pool-on/pool-off timings on it were comparing two arms in which
the pool did nothing — noise, reported as a 4-of-5 win. The chain bench
parks 3 blocks and reuses none. Only the dot bench, which allocates a
scalar-return cell per dispatch, actually recycles.

That census line is also what found the two defects below. It had been
printing in zero logs: `gpu_event_census::exit_summary` sat behind
`dispatch_timing::report`'s `calls == 0` early return, and `CALLS` only
moves on the `submitMethod` path, so no `--gpu` run printed it.

### The drop-time wait (fixed)

`DeviceBuffer::drop` called `ev.synchronize()` when the buffer's
last-write event had not fired, to widen the pool's admission. The
thread that drops a per-dispatch buffer is the thread submitting the
next dispatch, so that wait serialised the chain — the same defect as
the per-launch host callback. It now retires only an already-fired
block. The drop hook is gated on `device_pool_enabled()` too, so
`CRATONVM_GPU_DEVICE_POOL=0` is a complete ablation rather than half of
one.

`cuda-bridge/tests/alloc_pool_it.rs` is the regression test for the
admission rule, with both negative controls run.

### Pinned H2D staging (removed)

Prototyped behind `CRATONVM_GPU_PINNED_H2D=1`, then measured against
the pageable path by `cuda-bridge/tests/transfer_bandwidth_it.rs`:
10-23% SLOWER at every size from 1 to 128 MiB, because the host memcpy
into the pinned slab costs more than the faster DMA saves. The flag,
the pool and the staging branch are gone.

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
