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

## The benchmark session

Once `bringup.sh` is green, the measurement is one command on the box:

```bash
bash tools/gpu/amd-remote/go.sh            # rows 1-5, longer div chains, Llama 1B and 8B
bash tools/gpu/amd-remote/go.sh --quick    # rows 1-5 only, 3 reps
```

`go.sh` starts with a **pre-flight that refuses to run on a busy box**: GPU
use above `IDLE_PCT` (5%), or somebody else's `cargo bench` / craton-bolt /
`rocprof` job. A benchmark that starts on a shared GPU measures the
neighbour and disturbs the neighbour's own numbers. `FORCE=1` overrides it,
loudly, and is a decision for a person.

It calls `bench-rows.sh` (HotSpot | TornadoVM-OpenCL | CratonVM-HIP for the
five array-kernel rows) and `bench-llama.sh` (GPULlama3 decode tok/s,
interleaved rounds, median). Both:

* generate every arm of a division-chain row from ONE source at ONE length
  (`gen-chain.py`) — the committed double chain has 59 statements in the
  HotSpot/CratonVM class and 62 in the TornadoVM twin while both claim 64;
* report CratonVM as `ENGAGED=0` instead of a speedup when the kernel never
  dispatched (a CPU fallback prints a correct checksum), and
* compare checksums against HotSpot — a speed number whose checksum differs
  is not a result.

Prerequisites, all env-overridable: a CratonVM build with `gpu-driver-hip`
at `target-fast/release/cratonvm` (a no-LTO build, minutes to rebuild),
JDK 25, a TornadoVM 5.2.0-jdk25 **OpenCL** SDK (TornadoVM has no HIP
backend), `~/craton-gpu.jar`, `~/GPULlama3.java` with `classes-craton`, and
GGUF models in `~/models`.

## What TornadoVM can and cannot do here (verified, not assumed)

The TornadoVM column is the **OpenCL** backend over ROCm (it has no HIP
backend). Findings, reproduced on a second machine (Windows, RTX 2060 over
NVIDIA OpenCL) so they are not artefacts of the AMD box or its drivers:

* **int -> long reductions are unimplemented in 5.2.0** (`ReduceGPUSnippets`
  reaches `forBoxed`). Checked on five shapes: `long->long`, `int->int` and
  `long*long->long` compile and are correct; `int->long` and `int*int->long`
  do not. So the dot-product row's "unimplemented" is TornadoVM's, not ours.
* Our committed 300-statement dot twin *also* crashes TornadoVM's scheduler
  (`MAX_TASKS = 256` overrun), which masks the real error above. The runner
  therefore uses the single-statement twin and reports the true cause.
* **kfusion's TornadoVM arm cannot run on 5.2.0 OpenCL**: the `raycast`
  kernel makes `OCLAssembler.getAbsoluteIndexFromValue` throw
  `StringIndexOutOfBoundsException` (a constant where it expects a variable).
  Same on both machines; `-Dtornado.compiler.fullInlining=False` is no way
  out, kfusion fails earlier without it.
* **The `dotl` row exists because of that.** It is the same dot product as
  `long[]*long[] -> long`, with identical source in all three arms (300
  serial adds into a loop-local `q`, then ONE accumulate statement), so the
  comparison is speed rather than one `n/a`. Verified on TornadoVM locally:
  its checksum equals HotSpot's.
* TornadoVM's ray-tracer checksum differs from HotSpot's (not bit-exact);
  the report flags it.

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
