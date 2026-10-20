# Running CratonVM's HIP backend on a remote AMD box

Everything in this tree was built and tested against HIP without any
AMD hardware. This directory is how you find out what that was worth,
with the fewest possible minutes of GPU time spent finding out.

## The two commands

```bash
# Everything: copy, build, test, measure. ~15-30 min, mostly the build.
bash tools/gpu/amd-remote/deploy.sh <host> <ssh-key> [user]

# Just "does any of this work at all". Stops after the first real
# kernel launch.
bash tools/gpu/amd-remote/deploy.sh <host> <ssh-key> [user] --quick
```

Both leave `./amd-bringup.md` and `./hip-dump/` (the rendered HIP C++)
in the current directory when they finish.

## What the remote box needs

| | Needed for | If missing |
|---|---|---|
| **Linux** | everything | `cubecl-hip-sys` is Linux-only; there is no Windows path |
| **ROCm** (`/opt/rocm`, `hipconfig`) | stages 3-6 | stages 0-2 still run and still mean something |
| **Rust** (`rustup`) | everything | stage 1 fails immediately and says so |
| **A JDK** (`javac`) | stages 4-6 | stages 0-3 still run; the Java pipeline is skipped |
| `g++` | the host-shim oracle | skipped, loudly |

Nothing needs a CUDA toolkit, libclang or an AMD SDK at build time.
`cubecl-hip-sys` ships pre-generated bindings and `dlopen`s
`amdhip64`/`hiprtc` at first use, which is why `--features hip` also
compiles on a runner with no ROCm at all.

## The order, and why it is the order

`bringup.sh` runs six stages, each of which can only fail for reasons
the stages before it have ruled out. That is the point: a failure names
its own cause, instead of a benchmark printing a wrong number twenty
minutes in and leaving you to bisect the stack.

| | Stage | Answers |
|---|---|---|
| 0 | Environment | Is there a GPU, and does ROCm see it? |
| 1 | Build | Does the binary link? (Needs no ROCm — a failure here is code, not driver.) |
| 2 | No-driver guard | Is the remote tree the tree CI built? This test is green in public CI, so a failure here means the revision is wrong and nothing below should be read as a HIP result. |
| 3 | Host bridge | Does hiprtc compile, does a module load, does a hand-written kernel launch and return? |
| 4 | End to end | Does a **Java** method get lowered, rendered, compiled and run? |
| 5 | Correctness | Do the GPU's answers match the CPU's? |
| 6 | Measurement | Only now is a number worth having. |

Stage 3's kernels are hand-written on purpose. A lowering bug and a
host bug produce the same symptom — wrong numbers — and a stage that
exercises both at once cannot tell them apart.

Stage 5 is not a formality. This subsystem's characteristic failure is
not a crash; it is a kernel that silently did not run, or ran and
returned a plausible wrong number. `marshal-stress.sh` counts
host-to-device transfers **per kernel** and fails when a type never
dispatched, because correct answers from a CPU fallback prove nothing.

## What to look at when something fails

**Stage 3 fails, stage 0 found a device.** hiprtc's own build log is in
the error — that is the one thing this backend does better than the
CUDA path, which reports an opaque driver code. Read the message
before anything else.

**Stage 4 renders nothing.** Run with `--print-gpu-decisions`; the
analyzer prints one line per verdict. If a kernel is admitted but the
renderer refuses it, the message names the PTX instruction that has no
HIP form yet — that is a coverage gap, tracked in
`jit-hip/tests/golden/hip/manifest.txt`, not a defect.

**Stage 5 disagrees.** `hip-dump/` has the generated HIP C++ for every
kernel that was compiled. It is ordinary C++ and can be read directly.
The likely suspects, in order: the wavefront width (a reduction folded
for 32 lanes on a 64-lane part is wrong by half and looks plausible),
floating-point contraction (the hiprtc option list forbids it and
`hip_compile_options_are_ieee` pins that, but check the options the
build actually used), and denormal flushing.

**A number in stage 6 looks too good.** Check the transfer counts. The
fastest possible GPU kernel is one that never ran.

## Getting the architecture wrong

`--gpu-info` prints the `gfx` string and the wavefront width. If the
architecture is not in `jit-hip`'s table, the VM refuses to lower for it
and says so — it will not guess a wavefront width, because both
possible guesses are wrong on half the hardware and neither failure is
visible. Adding a row to `GFX_TABLE` in `jit-hip/src/target.rs` is a
one-line change; guessing is not.
