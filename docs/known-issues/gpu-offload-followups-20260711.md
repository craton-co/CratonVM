# GPU offload — open follow-ups after first real-hardware validation (2026-07-11)

**Context.** First systematic validation of the GPU offload stack on real hardware
(RTX 2060, sm_75, CUDA driver 591.86, `--features gpu-driver` build of dev). The
launch path works end-to-end: interpreter hook → `offload::try_dispatch` →
`dispatch_method_from_native` → `dispatch_async` → `cuda-bridge` →
cudarc `cuLaunchKernel`, with checksums matching HotSpot bit-for-bit on every
kernel tested. Two bugs found that day were fixed in-tree (invoke-cache
promotion killing repeat offloads; failure-flag drained after array writebacks).

**Update (2026-07-11, evening).** A second wave of hardware-validated work
closed items 1, 2, 4, 5, and 7 below, and made a real dent in item 6. Item 3
(async completion model) is partially done. Status is marked inline per item;
see `README.md`'s "GPU offload benchmarks" section and `docs/gpu/COMPARISON.md`
for the numbers this update is based on.

## 1. Reduction kernels never dispatch (void-return gate) — DONE

`try_dispatch` used to only launch kernels for methods whose descriptor ends in
`)V`. As of 2026-07-11 evening, `)I`/`)J` reduction kernels (`is_reduction: true`,
proven by the analyzer as a single counted loop with a scalar accumulator and no
array store) transparently dispatch: the scalar result is downloaded and pushed
onto the interpreter's operand stack (`DispatchOutcome::HandledWithValue` in
`vm/src/runtime/offload.rs::try_dispatch`). `)F`/`)D` reductions deliberately
stay on the CPU — GPU float atomic-add reorders the per-thread summation and is
not bit-identical to Java's sequential fp accumulation, so admitting them would
trade correctness for offload coverage. That is a documented design choice, not
an open gap.

**Bug found and fixed in the process.** The reduction PTX epilogue had been
emitting the 2-operand form `atom.global.add [ptr], value;` since the day it was
written. `ptxas` rejects that form with "Arguments mismatch for instruction
'atom'" — every reduction kernel failed module load and silently fell back to
CPU, which is exactly why this gate looked untested for so long. The fix is the
1-operand accumulate form, `red.global.add[.u64] [ptr], value;`
(`jit-cuda/src/lowering/emit.rs`). `ptxas` round-trip tests were added for all
six lowering shapes (`ptxas_round_trip_vector_add`, `_dot_reduction`,
`_ldc_constants`, `_offset_loops`, `_frem`, `_math_intrinsics` in
`jit-cuda/src/lowering.rs`) so a future epilogue regression fails the test suite
instead of silently degrading to CPU again.

**Hardware numbers** (`bench-gpu/GpuDotBench.java`, `sum += (long) a[i] * b[i]`
over `int[]`, RTX 2060, N = 2²⁴, checksum `DOT_CHECKSUM` verified bit-exact
against an independent decrementing-loop CPU oracle and against HotSpot):

| N | CratonVM-CPU | **CratonVM-GPU** | HotSpot C2 |
|---|---|---|---|
| 2²⁴ | 76 ms | **18 ms** | 7 ms |

Honest framing: this kernel is PCIe-bound (one int per element in, one 8-byte
scalar out) plus single-cell atomic contention on the accumulator, so the GPU
beats CratonVM's own CPU interpreter/JIT by 4.2× but does **not** beat
vectorized HotSpot C2 at this size. The point of this benchmark is not "GPU
wins" — it's that transparent reduction offload now works end-to-end at all,
completing the offload surface for a shape TornadoVM's own PTX backend
currently rejects: TornadoVM 4.0.1 throws
`TornadoInternalError: unimplemented` on the equivalent `@Reduce`-over-`LongArray`
kernel (`bench-tornado/TornadoDotBench.java`, mirroring the shipped
`ReductionAddFloats` idiom from `tornado-examples-4.0.1-jdk25.jar`). See
`docs/gpu/COMPARISON.md` for the full writeup.

## 2. JIT-compiled callers bypass the offload hook — DONE

The offload hook lives in the interpreter's `execute_invokestatic` slow path.
The earlier 2026-07-11 fix kept offload-eligible *call sites* out of the invoke
cache; this follow-up closes the structural gap described here: a new
JIT-caller admission gate (`vm/src/runtime/offload_jit_gate.rs`) denies
JIT/OSR compilation of any caller method whose body contains an
offload-eligible `invokestatic` site while `--gpu` is active, wired into all 5
JIT/OSR admission checks in `vm/src/runtime/interpreter.rs`
(`caller_blocks_jit_by_name`, called from the OSR-trigger check and every
compile-admission site). Interpreted callers now stay interpreted for the
lifetime of the eligible call site, so the hook is always consulted.
Hardware-validated: 100 hot repetitions of a caller loop at N = 2²² stay at a
steady 2 ms warm per call — before the gate, caller OSR silently degraded
offload back to CPU once the caller itself got hot enough to JIT.

## 3. `dispatch_async` is synchronous under the hood — PARTIALLY DONE

Real progress, but the fundamental completion model described below is
unchanged: `get()` is still the only thing that finalizes a submission.

**Landed:** a non-blocking device-side probe now backs `Native.futureIsDone` /
`Native.futureStatus` (`poll_submission_status` in `vm/src/runtime/offload.rs`,
built on a real `Event::query()` — `cuEventQuery` — added to `cuda-bridge`),
so calling `isDone()` actually asks the driver whether the kernel finished
instead of only reporting whatever the last blocking `get()` left behind. A
`cuLaunchHostFunc` host-callback primitive was also added to `cuda-bridge`
(`Stream::add_host_callback` in `cuda-bridge/src/stream.rs`) — the driver-level
building block for a future callback-driven future.

**Still open:** nothing spontaneously drives a submission to `Completed`
without a Java call. `isDone()`/`getNow()` are poll-based, not push-based — you
have to call them (or `get()`) for the status to update; there is still no
background thread or stream callback that flips a `GpuFuture` to done on its
own while the application does something else. Wiring the new
`cuLaunchHostFunc` primitive up to actually complete a `GpuFuture` from a
driver-thread callback, with no Java-side poll required, remains open work.

## 4. Small-array thread over-launch (min 2²⁰ threads per launch) — DONE

`dispatch_async` previously computed `work = max(runtime_work,
signature.estimated_work)` where `estimated_work` is a fixed `1 << 20`
placeholder for every counted-loop kernel, so any kernel over a smaller array
still launched 1,048,576 threads. Fixed: `runtime_work` now wins outright when
non-zero; `estimated_work` is only used as the scalar-only sentinel-0 fallback
(`vm/src/runtime/offload.rs`, the `work = if runtime_work > 0 { runtime_work }
else { ... }` branch). A kernel over N elements now launches N threads, not
`max(N, 2^20)`.

## 5. Occupancy-tuned block size is dead code — DONE

`dispatch_async` now uses the occupancy-tuned launch config
(`cuOccupancyMaxPotentialBlockSize` via `DeviceModule::elementwise_for_kernel`
in `cuda-bridge/src/backend_cuda.rs`) instead of the fixed 256-thread
`LaunchConfig::elementwise` block. Rolled out alongside the other dispatch
changes in this update; landed together with the thread-floor fix in item 4
above.

## 6. Analyzer/lowering coverage gaps — MOSTLY DONE

Closed this update:

- **`ldc`/`ldc_w`/`ldc2_w`** — Integer/Float/Long/Double constant-pool loads
  are now resolved against the constant pool and lowered as PTX immediates
  (`jit-cuda/src/analyzer.rs`, `jit-cuda/src/lowering/emit.rs`, "AUDIT C31
  follow-up"). Any int constant outside `sipush` range, or any float/double/long
  literal, is admitted instead of rejecting the whole method.
- **`frem`/`drem`** (IEEE remainder) — real PTX lowering
  (`Emitter::frem_f32`/`drem_f64`), gated behind the existing
  `AdmissionHint::ALLOW_DIV_BY_ZERO` hint (exact only for bounded quotients —
  see `docs/gpu/annotations.md`).
- **`lcmp`/`fcmpl`/`fcmpg`/`dcmpl`/`dcmpg`** — bit-exact `setp`/`selp` PTX
  lowering including the NaN-result asymmetry between the `l`/`g` variants.
  The analyzer admits the value-producing comparison opcode itself; a compare
  that immediately *feeds a branch* (the overwhelmingly common javac idiom)
  still rejects at the branch opcode — no false eligibility was introduced.
- **Non-zero-start counted loops** — `i = K; i < bound; i++` with `K >= 0` and
  an `ldc`-sourced `K` is now recognized (`jit-cuda/src/lowering/loop_recog.rs`);
  the emitter folds `K` into the induction variable.
- **`invokestatic` intrinsics** — the PHASE1-GUESS "admit any invokestatic"
  hole is closed. `AdmissionHint::ALLOW_INTRINSIC_CALLS` now resolves the
  constant-pool callee against a curated table
  (`analyzer::resolve_math_intrinsic`) and only admits a call that actually
  lowers; see `docs/gpu/annotations.md` for the full supported/excluded list.

Still open (unchanged from the original report, scope reduced):

- 2-D / nested loops still reject.
- General branches (anything other than a compare-and-branch loop guard) still
  reject.
- `)F`/`)D` reductions still stay on CPU by design — see item 1.

## 7. No hardware CI — SCAFFOLDING DONE, runner enrollment pending

`.github/workflows/gpu-selfhosted.yml` (weekly, self-hosted GPU runner label)
and `bench-gpu/ci-gate.sh` (runs the `bench-gpu/` suite and checks checksums,
same as the manual comparison scripts) now exist and are ready to catch a
regression like the invoke-cache bug or the `atom.global.add`/`ptxas` failure
in item 1 the day it lands. What's still missing is the self-hosted runner
itself — no CUDA box is enrolled against the workflow's runner label yet, so
the job is defined but has nothing to execute on.

## Benchmark snapshot backing this doc

See `bench-gpu/results/` (2026-07-11 files) and the README "GPU offload"
section for the CratonVM-vs-TornadoVM-vs-HotSpot numbers, including the
div-chain kernel where the GPU beats even auto-vectorized HotSpot C2 by ~70×.
The 2026-07-11 evening update added `bench-gpu/GpuDotBench.java` (reduction,
item 1), `bench-gpu/GpuLdcBench.java` (ldc constants, item 6), and their
TornadoVM twin `bench-tornado/TornadoDotBench.java`; see the README's "Update
2026-07-11 (evening)" paragraph and `docs/gpu/COMPARISON.md` for those
numbers.
Box caveat: measurements taken on a machine with ~25-30% background CPU load
(known infection, see session notes); GPU-side timings are largely unaffected
(GPU idle), CPU baselines are pessimistic by roughly that margin.
