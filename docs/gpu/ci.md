# GPU offload — real-hardware CI gate

This page covers the weekly self-hosted job that exercises GPU offload on
actual CUDA hardware. For everything else about the feature (build modes,
CLI flags, architecture), see [`README.md`](README.md).

## Why this exists

Every real-GPU test in this tree is `#[ignore]`d or `gpu-it`-gated (see the
completed follow-ups record,
item 7), and [`cuda-bridge.yml`](../../.github/workflows/cuda-bridge.yml)
only compiles the `cuda` Cargo feature on a public `ubuntu-latest` runner —
it links against `cudarc` but never opens a device or launches a kernel. So
until this job existed, nothing in CI could actually catch a GPU-offload
regression; only whoever happened to run `bench-gpu/` by hand on real
hardware would notice.

That gap was not theoretical. During the first systematic real-hardware
validation (RTX 2060), an invoke-cache promotion bug was found
that made offload **silently stop dispatching to the GPU after the first
call at a call site** — the program kept running, produced correct output
(the CPU fallback is correct, just slow), and nothing but a timing
comparison against a known-good GPU baseline would have flagged it. A
checksum-only gate would have passed throughout; only pairing checksums
*with* a timing bound catches this class of bug. That's the shape every
gate below follows.

## What the gate covers

Driven by [`bench-gpu/ci-gate.sh`](../../bench-gpu/ci-gate.sh), invoked from
[`.github/workflows/gpu-selfhosted.yml`](../../.github/workflows/gpu-selfhosted.yml):

| Gate | Checks | Why |
|---|---|---|
| a. `--gpu-info` | Exit 0 **and** a `device N: ...` line in the output | `--gpu-info` exits 0 both when a device is found and when the driver is missing (see `vm-cli/src/main.rs`) — exit code alone can't tell "healthy" from "driver fell off the box," so the device line is required too. |
| b. `GpuWarm f 2^24 5` under `--gpu` | `SAMPLE` matches the same class run on HotSpot, **and** `warm_ms < 500` | This is the invoke-cache-regression detector. A silent fallback to CPU still computes the right `SAMPLE` (the CPU path is correct, just slow) — only the timing bound catches it. Observed CPU-fallback time at this size is ~2000-3400ms; 500ms leaves roughly a 4-6x margin above real GPU warm time and a 4-7x margin below the fallback floor, so a busy/noisy runner won't false-alarm but an actual regression to CPU cannot sneak under the bar. |
| c. `GpuCompute` at 2^26 under `--gpu` | `COMPUTE_CHECKSUM` matches HotSpot | Pure correctness: the compute-heavy kernel (96 multiply-adds/element) must produce bit-identical results to the CPU reference. Catches PTX-lowering / marshalling bugs that a timing-only check would miss. |
| d. `BoundsDeopt2` under `--gpu --nojit` | Output contains `THROWN java.lang.ArrayIndexOutOfBoundsException` | Bounds-deopt integrity: the GPU kernel's per-access bounds check must still force a fallback to a correct CPU path that raises the required exception, not swallow it. `--nojit` is pinned deliberately — a *separate*, already-tracked JIT bounds-check-elimination bug can silently swallow this same exception with the JIT on (see `docs/known-issues/jit-bce-multi-array-oob-store-20260711.md`), and this gate exists to watch the GPU deopt path, not re-litigate that JIT bug. |
| e. `GpuProbe` `DOT_CHECKSUM` + engagement | `DOT_CHECKSUM` matches HotSpot **and** `dotReduce` really dispatched | **On by default since 2026-09-05.** It was off for two months on the premise that reduction dispatch had not shipped; that premise was stale — `)I`/`)J` reductions have dispatched via `DispatchOutcome::HandledWithValue` since 2026-07-11, confirmed on an RTX 2060 on 2026-09-05. The checksum alone still cannot gate it: `dotReduce` computes the same answer on the CPU, so a run that never offloaded prints the right checksum. Verified both ways in one binary — with offload refused on size the checksum is *still* correct and only the engagement witness goes to zero. So the gate also requires the per-kernel `H2D=… (GpuProbe.dotReduce…)` line that `CRATONVM_GPU_TRACE_BYTES=1` emits (needs `RUST_LOG=cratonvm_vm::runtime::offload=info` too — it is a `tracing::info!`). The witness is the line's **presence**, never a positive byte count: `vaddMap` runs over the same arrays first, so the residency cache correctly suppresses the re-upload and `dotReduce` legitimately reports `H2D=0 bytes`. |

Each gate prints a `PASS: ...` or `FAIL: ...` line; the script exits
non-zero if any enabled gate fails, so the workflow step (and therefore the
job) goes red.

## Enrolling a self-hosted GPU runner

The workflow targets `runs-on: [self-hosted, gpu]`. To add a runner:

1. On the target machine (needs an NVIDIA GPU, the CUDA Toolkit, and the
   same MSVC Build Tools used by `build-gpu-driver.bat`), go to the repo's
   **Settings → Actions → Runners → New self-hosted runner** and follow
   GitHub's setup script for the OS.
2. When configuring the runner (`config.cmd`/`config.sh`), add both labels:
   `--labels self-hosted,gpu`. The `gpu` label is what this workflow's
   `runs-on` matches on — a plain `self-hosted` runner without it will
   never pick up this job.
3. Make sure a real JDK is installed and its path matches (or is pointed at
   via the `GPU_CI_JDK_HOME` repository/environment variable — see the
   `env:` block in `gpu-selfhosted.yml`) — the job does not provision a JDK
   itself, it uses whatever real JDK already lives on the box, the same way
   every `bench-gpu/*.sh` script does.
4. Install/verify Git for Windows (Git-Bash) is on `PATH` — every step in
   the workflow runs under `shell: bash`.
5. Register the runner as a Windows service (or otherwise keep it always-on)
   so the weekly cron trigger has something to land on.

Only one GPU box is assumed. If you add more, keep the `gpu` label common
across them and GitHub will queue/distribute across whichever is idle.

## Running it locally

```bash
# from the repo root, after a gpu-driver build:
#   CARGO_TARGET_DIR=target-gpu cargo build --release -p cratonvm-cli \
#     --bin cratonvm --features gpu-driver
# and after compiling the fixtures it needs:
#   "$JDK/bin/javac" -d bench-gpu bench-gpu/GpuWarm.java bench-gpu/GpuCompute.java bench-gpu/GpuProbe.java
#   "$JDK/bin/javac" -d test_classes/gpu test_classes/gpu/EligibleVectorAdd.java test_classes/gpu/BoundsDeopt2.java

CV_GPU="C:/craton/CratonVM/target-gpu/release/cratonvm.exe" \
JDK="C:/Program Files/Java/jdk-25" \
bash bench-gpu/ci-gate.sh
```

All inputs are optional environment variables with defaults matching the
existing `bench-gpu/run-gpu-comparison.sh` / `run-gpu-warm.sh` conventions:

| Variable | Default | Meaning |
|---|---|---|
| `CV_GPU` | `$ROOT/target-gpu/release/cratonvm.exe` | The `--features gpu-driver` binary under test. |
| `JDK` | `C:/Program Files/Java/jdk-25` | Real JDK home. |
| `HS` | `$JDK/bin/java.exe` | HotSpot binary used as the reference implementation for checksums/samples. |
| `GO` | `$ROOT/bench-gpu` | Classpath for the `bench-gpu/*.java` fixtures. |
| `TG` | `$ROOT/test_classes/gpu` | Classpath for the `test_classes/gpu/*.java` fixtures. |
| `GATE_REDUCTION` | `1` | Gate e (dot-reduction checksum + engagement). Set to `0` to skip it. |

Exit code is `0` when every enabled gate passes, `1` otherwise — safe to
wire into any other harness as a plain pass/fail check.

## Triggers

Scheduled weekly (Monday 06:00 UTC) plus `workflow_dispatch` for on-demand
runs. Deliberately **not** wired to `push`/`pull_request`: self-hosted GPU
capacity here is a single box, and letting arbitrary PR branches queue jobs
on it would starve the weekly regression gate and give a bad PR a way to
tie up the only GPU runner. `workflow_dispatch` covers "I just landed a
GPU-offload change and want to check it now" without needing to wait for
Monday.

The job is guarded with
`if: github.repository == 'craton-co/cratonvm'` so a fork — which has no
access to this repo's self-hosted runner pool — never leaves a scheduled or
dispatched run stuck queuing forever with no runner able to pick it up.
