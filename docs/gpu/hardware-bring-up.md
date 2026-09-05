# Bringing up a GPU box: what to build, what to run first, and what is waiting on hardware

[hardware-ci.md](hardware-ci.md) has said "no CUDA machine is currently
enrolled" since the workflow was written. This page is the other half of that
sentence: what someone with a CUDA device in front of them should do, in order,
and which open questions in this tree only hardware can answer.

Nothing here needs the self-hosted runner to be enrolled first. Enrolment is
step 5, not step 1 — the value is in the first run, not in the automation.

## 0. What you need

| | |
|---|---|
| GPU | any CUDA device the driver enumerates; the gates are correctness-and-checksum, not throughput-tiered |
| CUDA | **12.6** — `cudarc` is pinned `features = ["driver", "cuda-12060"]` in `cuda-bridge/Cargo.toml` |
| JDK | a real JDK 21+ (`JDK=…`); HotSpot from the same JDK is the differential oracle |
| `GPU_JAR` | **the `craton-gpu` annotations jar, which is NOT in this repository.** Several bench scripts open with `GPU_JAR="${GPU_JAR:?set GPU_JAR to the craton-gpu annotations jar}"`. The `craton.gpu` API classes (`GpuExecutor`, `GpuFuture`, …) live in that jar — `find` for `GpuExecutor.java` in this tree returns nothing, which has misled at least one investigation into concluding the API did not exist. |

### Which backend

`cuda-bridge` has two, and they are mutually exclusive (`compile_error!` if you
enable both):

* **`gpu-driver` (cudarc)** — the one to use. Works on Windows and Linux.
* **`gpu-driver-oxide` (NVlabs `cuda-core`)** — **Linux only.** `cuda-core`
  0.3.1 does not compile on Windows/MSVC: 17 signedness errors inside the crate
  itself, reproduced on 0.3.1 and on upstream `main`. Not our bug. A 13-cast fix
  is prepared in-tree at
  `docs/known-issues/gpu/0001-cuda-core-msvc-simt-flags.patch` and was verified
  working on real hardware after patching — see
  `docs/known-issues/gpu/cuda-core-msvc-enum-signedness-20260904.md`. If you are
  on Windows and want this backend, applying that patch to a vendored `cuda-core`
  (and submitting it upstream) is a real, bounded task.

```bash
# the binary every script below expects
cargo build --release -p cratonvm-cli --features gpu-driver
# → target/release/cratonvm[.exe]
```

Note the feature names differ by crate and it is easy to pick the wrong one:
`vm-cli` has `gpu` / `gpu-driver` / `gpu-driver-oxide`; `cratonvm-vm` has
`gpu-offload`; `cratonvm-native-builtins` has `gpu-offload` **with no
dependencies at all** (which is why its GPU unit tests run on a CPU-only box —
see step 4).

## 1. First run: does the box see a device

```bash
CV=target/release/cratonvm
"$CV" --gpu-info
```

`--gpu-info` **always exits 0** — "device found" and "no CUDA driver" are both
clean early exits. So the exit code proves nothing; what proves it is a line
matching `^device [0-9]+:`. `bench-gpu/ci-gate.sh`'s gate (a) checks exactly
that, and for that reason.

## 2. The highest-value single run: the submission drain

Run this before anything else. It is the newest correctness fix in the GPU
stack, it is the one with the shortest chain between "regressed" and "the host
runs out of memory", and it has a built-in negative control.

```bash
CV=target/release/cratonvm JDK=/path/to/jdk bash bench-gpu/submission-drain.sh
```

Asserts `registered > 0` (else the run never offloaded and proves nothing —
a no-device box would otherwise "pass"), `live_at_exit == 0`, and reports
`peak_live`, which must not exceed the launch count. That last one is the part
worth reading: a drain that only ran at exit would still show `live_at_exit=0`
with a peak of `launches × rounds`, so `peak_live` is what separates "drained"
from "drained too late to matter".

**Then run its negative control**, which is the point of the script:

```bash
CRATONVM_GPU_NO_SUBMISSION_DRAIN=1 CV=… JDK=… bash bench-gpu/submission-drain.sh
```

Every assertion must fail. If both arms pass, the script is not measuring what
it claims and that is a bigger finding than either result.

### Why this one first

`offload::SUBMISSIONS` had exactly one insert and one remove, and for a stretch
the remove had **no production caller**: `GpuExecutor.releaseSubmission(h)`
compiles to `Native.releaseFuture(h)`, which removed the entry from
`native-builtins`' own future table and stopped there. The offload registry,
keyed by the same handle, was never touched — so no program could drain it
however correctly written. `GpuAsyncChainBench`, which awaits and releases every
handle it takes, still reported `released=0 live_at_exit=2001`.

Fixed by `f6061f7b3`; the ownership contract is in
[async-api.md](async-api.md) under "Submission handles must be released", and
note its conclusion — closing the executor **cannot** sweep the registry,
because the map is process-global and a close-time drain-all would free another
executor's submissions.

The shim half of that forward is gated on CPU
(`release_future_forwards_the_drain_to_the_registry`). The device half is this
script, and it has only ever been run by hand.

## 3. The gate battery

```bash
CV_GPU=target/release/cratonvm JDK=… GO=bench-gpu TG=test_classes/gpu \
  bash bench-gpu/ci-gate.sh
```

Four gates plus one optional:

| gate | what it catches |
|---|---|
| a | `--gpu-info` sees a device (driver/hardware vanished) |
| b | `GpuWarm` warm-timing + correctness — a **silent offload-to-CPU** regression |
| c | `GpuCompute` checksum at 2²⁶ |
| d | `BoundsDeopt2` integrity under `--gpu --nojit` |
| e | dot-reduction checksum — **off by default** (`GATE_REDUCTION=1`), because reduction dispatch had not shipped when it was written. Worth re-checking whether that is still true. |

Gate (b) is the load-bearing one. It exists because a regression already slipped
through undetected: the 2026-07-11 invoke-cache promotion bug made offload
silently stop after the *first* call at a call site, and a checksum-plus-timing
gate on real hardware would have caught it the day it landed. Every real-GPU
test in the tree is `#[ignore]`d or gpu-it-gated, so this script is the only
thing standing between a silent offload regression and a green build.

The script is Windows-shaped by default (`ROOT=C:/craton/CratonVM`,
`cratonvm.exe`, `MSYS2_ARG_CONV_EXCL` to stop MSYS mangling `--gpu` and leading
slashes). Override the paths on Linux; keep the MSYS exports harmless either
way.

## 4. What you can check WITHOUT the device, to isolate a failure

If a gate above fails, this tells you whether the shim or the device side moved.
`cratonvm-native-builtins`' `gpu-offload` feature is declared `gpu-offload = []`
— **no dependencies** — so its GPU unit tests, including every
`MockNativeContext` contract, run anywhere:

```bash
cargo test -p cratonvm-native-builtins --features gpu-offload --lib -- craton_gpu
```

A green run here plus a red gate above puts the fault past the shim boundary.

## 5. Enrol the runner

`.github/workflows/gpu-selfhosted.yml` is written and waiting for a machine
with the right label; `bench-gpu/ci-gate.sh` is what it runs. See
[ci.md](ci.md) for the workflow structure. Do this last — after a manual gate
run has passed at least once, so a red first CI run means "the runner is
misconfigured" rather than "something in the tree is broken and we don't know
which".

## What is actually waiting on hardware

Ranked by what a single session could settle:

1. **The drain's device half** (step 2) — never run outside the session that
   fixed it. Cheapest, highest consequence.
2. **`GATE_REDUCTION`** — off since reduction dispatch "hasn't shipped yet".
   Either it ships and the gate turns on, or the comment is stale. One run says
   which.
3. **The breakeven/crossover surface** — `arithmetic-intensity-sweep-20260904.md`
   and `offload-crossover-and-min-work-20260904.md` are active, and
   `bench/gpu-breakeven-surface-20260905` merged on 2026-09-05. Whoever picks
   this up should read those two first; the sweep scripts are
   `bench-gpu/intensity-sweep.sh` and `bench-gpu/crossover-n.sh`.
4. **`cuda-core` on Windows** — apply the prepared patch, confirm on device,
   submit upstream. The report to send is already written
   (`cuda-core-msvc-upstream-report.md`).
5. **Runner enrolment** — the standing operational task
   ([hardware-ci.md](hardware-ci.md)).

## Two traps this tree has already paid for

* **A no-device box passes a badly-written GPU gate.** Every script here has an
  anti-vacuity assertion for that reason (`registered > 0`, the `^device N:`
  line, a checksum rather than an exit code). When you add one, add its
  anti-vacuity clause in the same commit.
* **`--gpu-info` exits 0 with no driver.** More generally: on this surface the
  exit status is rarely the answer. Read the output.
