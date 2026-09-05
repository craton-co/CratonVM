# Bringing up a GPU box: what to build, what to run first, and what is waiting on hardware

[hardware-ci.md](../../gpu/hardware-ci.md) has said "no CUDA machine is currently
enrolled" since the workflow was written. This page is the other half of that
sentence: what someone with a CUDA device in front of them should do, in order,
and which open questions in this tree only hardware can answer.

Nothing here needs the self-hosted runner to be enrolled first. Enrolment is
step 5, not step 1 — the value is in the first run, not in the automation.

**First full run: 2026-09-05**, RTX 2060 (sm_75), CUDA 13.3, driver 610.88,
Windows 11, JDK 25.0.3, `cratonvm-cli --features gpu-driver` at `b82da0607`.
Steps 1-4 all executed. Results are recorded inline below, and the two
things that run found are
[the concurrent-dispatch wrong answer](concurrent-dispatch-wrong-answer-20260905.md)
and a gate that had been switched off for two months on a stale premise
(step 3, gate e). Enrolment (step 5) is still not done.

## 0. What you need

| | |
|---|---|
| GPU | any CUDA device the driver enumerates; the gates are correctness-and-checksum, not throughput-tiered |
| CUDA | **12.6** — `cudarc` is pinned `features = ["driver", "cuda-12060"]` in `../../../cuda-bridge/Cargo.toml` |
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
  `0001-cuda-core-msvc-simt-flags.patch` and was verified
  working on real hardware after patching — see
  `cuda-core-msvc-enum-signedness-20260904.md`. If you are
  on Windows and want this backend, applying that patch to a vendored `cuda-core`
  (and submitting it upstream) is a real, bounded task.

```bash
# the binary every script below expects. NOTE the target dir: every
# bench-gpu/*.sh defaults to target-gpu/release/cratonvm.exe, the CI
# workflow sets CARGO_TARGET_DIR=target-gpu, and so does
# scripts/internal/build-gpu-driver.bat. Building into the default
# `target/` instead puts the binary where no script's default looks AND
# rebuilds the whole CPU tree under a different feature set.
CARGO_TARGET_DIR=target-gpu \
  cargo build --release -p cratonvm-cli --bin cratonvm --features gpu-driver
# → target-gpu/release/cratonvm[.exe]
```

Note the feature names differ by crate and it is easy to pick the wrong one:
`vm-cli` has `gpu` / `gpu-driver` / `gpu-driver-oxide`; `cratonvm-vm` has
`gpu-offload`; `cratonvm-native-builtins` has `gpu-offload` **with no
dependencies at all** (which is why its GPU unit tests run on a CPU-only box —
see step 4).

## 1. First run: does the box see a device

```bash
CV=target-gpu/release/cratonvm
"$CV" --gpu-info
```

`--gpu-info` **always exits 0** — "device found" and "no CUDA driver" are both
clean early exits. So the exit code proves nothing; what proves it is a line
matching `^device [0-9]+:`. `../../../bench-gpu/ci-gate.sh`'s gate (a) checks exactly
that, and for that reason.

2026-09-05, this box:

```
device 0: NVIDIA GeForce RTX 2060 (sm_75), 12.00 GiB
```

Note that the `cudarc` pin is `cuda-12060` while this box runs CUDA 13.3
with driver 610.88. That combination works — the driver API is backward
compatible — so the "CUDA 12.6" row in the table above is the version the
bindings are generated against, not a version the box has to match.

## 2. The highest-value single run: the submission drain

Run this before anything else. It is the newest correctness fix in the GPU
stack, it is the one with the shortest chain between "regressed" and "the host
runs out of memory", and it has a built-in negative control.

```bash
CV=target-gpu/release/cratonvm JDK=/path/to/jdk bash bench-gpu/submission-drain.sh
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

Every *drain* assertion must fail. `registered > 0` must still PASS in both
arms — it is the anti-vacuity clause, not a drain check, and a control arm
in which it failed would prove nothing about draining either. If both arms
pass the drain assertions, the script is not measuring what it claims and
that is a bigger finding than either result.

**Run 2026-09-05 — the device half is now settled.** Both arms, defaults
(`n=65536 launches=400 rounds=5`):

| arm | census | verdict |
|---|---|---|
| default | `registered=2001 released=2001 live_at_exit=0 peak_live=400` | 3/3 PASS |
| `CRATONVM_GPU_NO_SUBMISSION_DRAIN=1` | `registered=2001 released=0 live_at_exit=2001 peak_live=2001` | 3 FAIL + the overflow warning |

`peak_live=400` is exactly the launch count, so the registry is bounded by
the program's outstanding-chain length rather than by the life of the
process — the distinction `peak_live` exists to make. The control arm
reproduces the pre-fix numbers in the script's own header
(`released=0 live_at_exit=2001`) exactly, so the script discriminates.

### Why this one first

`offload::SUBMISSIONS` had exactly one insert and one remove, and for a stretch
the remove had **no production caller**: `GpuExecutor.releaseSubmission(h)`
compiles to `Native.releaseFuture(h)`, which removed the entry from
`native-builtins`' own future table and stopped there. The offload registry,
keyed by the same handle, was never touched — so no program could drain it
however correctly written. `GpuAsyncChainBench`, which awaits and releases every
handle it takes, still reported `released=0 live_at_exit=2001`.

Fixed by `f6061f7b3`; the ownership contract is in
[async-api.md](../../gpu/async-api.md) under "Submission handles must be released", and
note its conclusion — closing the executor **cannot** sweep the registry,
because the map is process-global and a close-time drain-all would free another
executor's submissions.

The shim half of that forward is gated on CPU
(`release_future_forwards_the_drain_to_the_registry`). The device half is this
script; before 2026-09-05 it had only ever been run by the session that
fixed the bug.

## 3. The gate battery

```bash
CV_GPU=target-gpu/release/cratonvm JDK=… GO=bench-gpu TG=test_classes/gpu \
  bash bench-gpu/ci-gate.sh
```

Five gates, all on by default since 2026-09-05:

| gate | what it catches | 2026-09-05 |
|---|---|---|
| a | `--gpu-info` sees a device (driver/hardware vanished) | PASS |
| b | `GpuWarm` warm-timing + correctness — a **silent offload-to-CPU** regression | PASS, `warm_ms=7-8` across runs vs HotSpot 1227-1463 |
| c | `GpuCompute` checksum at 2²⁶ | PASS, 209 ms vs HotSpot 5132 |
| d | `BoundsDeopt2` integrity under `--gpu --nojit` | PASS |
| e | dot-reduction checksum **and** a dispatch witness | PASS |

Gate (e) was off for two months on the premise that reduction dispatch
"hadn't shipped". **That premise was stale**: `)I`/`)J` reductions have
dispatched through `DispatchOutcome::HandledWithValue` since 2026-07-11,
and the follow-ups item it cited had been marked DONE that same day. One
run on hardware was enough to show the premise was false — but not enough
to turn the gate on, for the reason below.

Turning it on needed more than flipping the default, because the gate as
written would have passed vacuously — `dotReduce` computes the same answer
on the CPU. Measured in one binary, one flag apart:

| arm | `DOT_CHECKSUM` | dispatch witness |
|---|---|---|
| `--gpu` | correct | present |
| `--gpu --gpu-min-work 999999999` | **still correct** | absent |

So gate (e) now also requires the per-kernel `H2D=… (GpuProbe.dotReduce…)`
line, and both arms of *that* were verified: driving the gate through a
wrapper that refuses offload on size makes it FAIL on a correct checksum.
Two traps worth knowing if you touch it — the witness must be the line's
**presence**, not a positive byte count (`vaddMap` runs over the same
arrays first, so the residency cache correctly suppresses the re-upload
and `dotReduce` legitimately reports `H2D=0 bytes`); and a process-wide
launch census will not do, because `vaddMap` offloads in the same run and
would keep any whole-process counter non-zero regardless.

The witness needs **both** `CRATONVM_GPU_TRACE_BYTES=1` and
`RUST_LOG=cratonvm_vm::runtime::offload=info`. It is a `tracing::info!`:
the flag alone has no subscriber and prints nothing, which is a quiet way
to conclude a kernel never dispatched when it did.

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

One fixture gotcha: `BoundsDeopt2.java` calls `EligibleVectorAdd`, so
compiling it needs its own directory on the classpath —
`javac -cp "$JAR;test_classes/gpu" -d test_classes/gpu …`, or just compile
both files together as `gpu-selfhosted.yml` does.

## 3b. The rest of what CI runs — do not stop at `ci-gate.sh`

`ci-gate.sh` is **one of six** steps in `gpu-selfhosted.yml`. Running only
it and then enrolling defeats the reason this page puts enrolment last: a
red first CI run would still be ambiguous. Run these too, all with the
same `CV`/`JDK`/`TG`:

| script | what it covers | 2026-09-05 |
|---|---|---|
| `bench-gpu/runtime-stress.sh` | concurrent dispatch, residency coherence, read-only inputs, aliasing, repeat submit, deopt-then-continue, bulk writes | **FAILED — see below** |
| `bench-gpu/marshal-stress.sh` | all six element types × zero-copy and staged transfer, with a per-kernel engagement census | PASS (all six offloaded) |
| `bench-gpu/residency-gc.sh` | residency cache survives relocation, **once per collector** | PASS on ZGC, G1, Generational; `re-keyed > 0` on each, so non-vacuous |
| `bench-gpu/jit-writer-stale.sh` | a compiled array writer must not leave the residency cache stale | PASS |
| `cargo test -p cratonvm-cuda-bridge --features cuda --test concurrent_dispatch_it -- --ignored` | bridge-level concurrency | PASS (2) |
| `cargo test -p cratonvm-cuda-bridge --features cuda --test stream_ordering_it -- --ignored` | cross-stream ordering against the real driver | PASS (3) |

**`runtime-stress.sh`'s `concurrent` scenario returns a wrong answer about
10% of the time.** Full write-up, rates, controls and the amplifier that
takes it to 53%:
[concurrent-dispatch-wrong-answer-20260905.md](concurrent-dispatch-wrong-answer-20260905.md).
Note what this means for the ordering advice above: the gate battery was
fully green while a real correctness defect sat one script away. `ci-gate.sh`
has no concurrent-dispatch shape at all.

It also means the first enrolled CI run will be **red**, and legitimately
so. Fix or quarantine that scenario before step 5, or the "red means the
runner is misconfigured" reasoning this page is built on stops holding.

## 4. What you can check WITHOUT the device, to isolate a failure

If a gate above fails, this tells you whether the shim or the device side moved.
`cratonvm-native-builtins`' `gpu-offload` feature is declared `gpu-offload = []`
— **no dependencies** — so its GPU unit tests, including every
`MockNativeContext` contract, run anywhere:

```bash
cargo test -p cratonvm-native-builtins --features gpu-offload --lib -- craton_gpu
```

A green run here plus a red gate above puts the fault past the shim boundary.

2026-09-05: **44 passed, 0 failed**, including
`release_future_forwards_the_drain_to_the_registry` — the shim half of the
step-2 drain. So the concurrent-dispatch defect found in step 3b is past
the shim boundary, in the VM offload runtime.

## 5. Enrol the runner

`../../../.github/workflows/gpu-selfhosted.yml` is written and waiting for a machine
with the right label; `../../../bench-gpu/ci-gate.sh` is the first of the six
steps it runs (see step 3b for the rest). See
[ci.md](../../gpu/ci.md) for the workflow structure. Do this last — after a manual gate
run has passed at least once, so a red first CI run means "the runner is
misconfigured" rather than "something in the tree is broken and we don't know
which".

**Still not done as of 2026-09-05**, and it should stay that way until the
`concurrent` scenario in step 3b is fixed or quarantined: enrolling now
would produce a red first run caused by a real tree defect, which is the
one thing the ordering above exists to prevent.

## What is actually waiting on hardware

Ranked by what a single session could settle. **Items 1 and 2 were settled
on 2026-09-05** and are kept here, struck through, because what they cost
is the useful part of the estimate.

1. ~~**The drain's device half** (step 2).~~ **Settled** — both arms, one
   run each, roughly a minute. It was correctly ranked cheapest-and-highest:
   it took less time than building the binary.
2. ~~**`GATE_REDUCTION`**.~~ **Settled — the comment was stale**, by two
   months. But "one run says which" was optimistic: the run says the
   checksum matches, and the checksum matches whether or not anything
   offloaded. Turning the gate on took a second measurement (the engagement
   witness) and a third (that the witness discriminates). See step 3.
3. **The breakeven/crossover surface** — largely settled already, and not by
   this page. `../../gpu/arithmetic-intensity-sweep-20260904.md` and
   `../../gpu/offload-crossover-and-min-work-20260904.md` were both measured
   on this same box on 2026-09-04, and `bench/gpu-breakeven-surface-20260905`
   merged into `dev` on 2026-09-05 with the conclusion that **no scalar
   `--gpu-min-work` is correct** — the ops=1 recommendation to raise it to
   ~32,768 would create new losses at ops=4 and ops=16. Read those before
   re-running anything; the sweep scripts are
   `../../../bench-gpu/intensity-sweep.sh` and `../../../bench-gpu/crossover-n.sh`.
   `--gpu-min-work` still defaults to 4096, which those pages show is 3-5x
   too eager at ops=1, so the open part is what replaces it, not what it costs.
4. **The concurrent-dispatch wrong answer** — new on 2026-09-05, and now the
   highest-value open item on this list:
   [concurrent-dispatch-wrong-answer-20260905.md](concurrent-dispatch-wrong-answer-20260905.md).
   It has a 53% repro recipe, clean controls, a collector split and a ruled-out
   shortlist; what it does not have is a diagnosis.
5. **`cuda-core` on Windows** — apply the prepared patch, confirm on device,
   submit upstream. The report to send is already written
   (`cuda-core-msvc-upstream-report.md`). Note `docs/cuda-core-linux-verified-20260905`
   exists, so the Linux half is done.
6. **Runner enrolment** — the standing operational task
   ([hardware-ci.md](../../gpu/hardware-ci.md)). Blocked on item 4: see step 5.

## Four traps this tree has already paid for

* **A no-device box passes a badly-written GPU gate.** Every script here has an
  anti-vacuity assertion for that reason (`registered > 0`, the `^device N:`
  line, a checksum rather than an exit code). When you add one, add its
  anti-vacuity clause in the same commit.
* **`--gpu-info` exits 0 with no driver.** More generally: on this surface the
  exit status is rarely the answer. Read the output.
* **A gate switched off "until X ships" outlives X.** Gate (e) sat off for
  two months after the thing it was waiting for landed, and the follow-ups
  item it cited had been marked DONE the same day the feature shipped. A
  disabled gate has no failing run to remind anyone it exists, so nothing
  ever prompted a re-read. If you switch a gate off pending some other work,
  the *other* work's page is where the reminder has to live.
* **A green battery is not a green surface.** On 2026-09-05 all five gates
  passed while `runtime-stress.sh` — a sibling script in the same CI job —
  returned wrong answers 10% of the time. `ci-gate.sh` simply has no
  concurrent-dispatch shape. Before concluding the GPU path is healthy,
  check what the scripts you ran actually cover, not just that they passed.

## Known residual in this page's neighbourhood

Several public files still cite
`docs/known-issues/gpu-offload-followups-20260711.md`, which has moved
into the internal tree at `fixed-suite-bugs/` (a path relative to that
tree's own root) and so is stripped from public history —
`bench-gpu/ci-gate.sh`, `bench-gpu/run-gpu-comparison.sh`,
`bench-gpu/run-gpu-warm.sh`, `.github/workflows/gpu-selfhosted.yml`,
`cratonvm-embed/README.md` and `CHANGELOG.md` among them. Left alone here
deliberately: fixing one or two of ten identical references makes the tree
less consistent, not more. It wants one sweep, not a drive-by.
