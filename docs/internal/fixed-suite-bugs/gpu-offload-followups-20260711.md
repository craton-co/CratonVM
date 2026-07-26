# GPU offload — completed follow-ups after first real-hardware validation (2026-07-11)

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
see `../../../README.md`'s "GPU offload benchmarks" section and `../../gpu/COMPARISON.md`
for the numbers this update is based on.

**Update (2026-07-12).** All code follow-ups in this report are closed. In
particular, item 6 now lowers acyclic `if`/`else` control flow inside a counted
loop body with explicit basic blocks, PTX labels, predicate branches, and
per-block JVM-state merge registers. The implementation intentionally keeps
early exits and interior backward edges on the CPU: they change the one-thread
per-iteration work mapping rather than being ordinary branch joins. The
fixture `EligibleBranchingLoop.java` covers both an `if`/`else` local merge and
a one-arm branch; `ptxas_round_trip_branching_loops` is the hardware-assembler
regression hook.

Item 3 (async completion model) is also closed: the `cuLaunchHostFunc` host
callback now drives a submission to `Completed`/`Failed` via a background
completion reaper, with no Java-side poll required. This report is retained
under `..` as the completed record; self-hosted runner enrollment
in item 7 is operational follow-up, not an unresolved code defect.

## Feature documentation

The durable feature documentation derived from this report lives under
`../../gpu`:

- [Integer and long reductions](../../gpu/reductions.md)
- [JIT caller gate](../../gpu/jit-caller-gate.md)
- [Callback-driven async completion](../../gpu/async-completion-reaper.md)
- [Runtime launch work sizing](../../gpu/launch-work-sizing.md)
- [Occupancy-tuned launch configuration](../../gpu/occupancy-launch-config.md)
- [Numeric constant-pool lowering](../../gpu/lowering-constants.md)
- [Floating-point remainder lowering](../../gpu/lowering-fp-remainder.md)
- [Comparison opcode lowering](../../gpu/lowering-comparisons.md)
- [Non-zero-start counted loops](../../gpu/lowering-offset-loops.md)
- [Curated Math/StrictMath intrinsics](../../gpu/lowering-intrinsics.md)
- [Rectangular two-dimensional loops](../../gpu/lowering-nested-loops.md)
- [Acyclic loop-body branch lowering](../../gpu/lowering-branches.md)
- [Self-hosted GPU CI scaffolding](../../gpu/hardware-ci.md)

This file remains the dated validation narrative and benchmark snapshot; the
feature documents above are the maintained technical entry points.

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
(`../../../jit-cuda/src/lowering/emit.rs`). `ptxas` round-trip tests were added for all
six lowering shapes (`ptxas_round_trip_vector_add`, `_dot_reduction`,
`_ldc_constants`, `_offset_loops`, `_frem`, `_math_intrinsics` in
`../../../jit-cuda/src/lowering.rs`) so a future epilogue regression fails the test suite
instead of silently degrading to CPU again.

**Hardware numbers** (`../../../bench-gpu/GpuDotBench.java`, `sum += (long) a[i] * b[i]`
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
kernel (`../../../bench-tornado/TornadoDotBench.java`, mirroring the shipped
`ReductionAddFloats` idiom from `tornado-examples-4.0.1-jdk25.jar`). See
`../../gpu/COMPARISON.md` for the full writeup.

## 2. JIT-compiled callers bypass the offload hook — DONE

The offload hook lives in the interpreter's `execute_invokestatic` slow path.
The earlier 2026-07-11 fix kept offload-eligible *call sites* out of the invoke
cache; this follow-up closes the structural gap described here: a new
JIT-caller admission gate (`../../../vm/src/runtime/offload_jit_gate.rs`) denies
JIT/OSR compilation of any caller method whose body contains an
offload-eligible `invokestatic` site while `--gpu` is active, wired into all 5
JIT/OSR admission checks in `../../../vm/src/runtime/interpreter.rs`
(`caller_blocks_jit_by_name`, called from the OSR-trigger check and every
compile-admission site). Interpreted callers now stay interpreted for the
lifetime of the eligible call site, so the hook is always consulted.
Hardware-validated: 100 hot repetitions of a caller loop at N = 2²² stay at a
steady 2 ms warm per call — before the gate, caller OSR silently degraded
offload back to CPU once the caller itself got hot enough to JIT.

## 3. `dispatch_async` is synchronous under the hood — DONE

The 2026-07-11 evening wave landed the non-blocking poll half: `Native.futureIsDone`
/ `Native.futureStatus` (`poll_submission_status` in `../../../vm/src/runtime/offload.rs`,
built on a real `Event::query()` — `cuEventQuery` — added to `cuda-bridge`),
plus a `cuLaunchHostFunc` host-callback primitive (`Stream::add_host_callback`
in `../../../cuda-bridge/src/stream.rs`) that set a best-effort `device_done` flag —
the driver-level building block for a real callback-driven future, not yet
wired to anything that acted on its own.

**Closed this update (2026-07-12).** That callback now drives completion
spontaneously, with no Java call required. A process-wide completion reaper
thread (`ensure_completion_reaper_started`/`completion_reaper_loop` in
`../../../vm/src/runtime/offload.rs`, modelled on the existing background-JIT-compiler
worker: `Once`-guarded singleton spawn, deliberately unregistered with the GC
thread registry since it never holds a managed `ObjectRef` on its own stack)
wakes on a condvar the host callback notifies (`enqueue_completion`) and
runs `finalize_submission` itself — draining writebacks, releasing the
GC-critical guard, and flipping the submission to `Completed`/`Failed` —
entirely off the mutator. `isDone()`/`get()` now frequently just read an
already-terminal status instead of doing any work at all.

Fixing this exposed a real attach-order race: `dispatch_async` used to
register the host callback *before* its caller
(`dispatch_method_from_native_on_stream`) attached the pending writebacks
to the submission, which was harmless when only a flag-setting callback
could run early but became a genuine bug once the callback could trigger a
real finalize — a callback firing first would see no writebacks to drain
and give up, permanently orphaning the submission. Fixed by moving the
writeback/GC-guard attachment inside `dispatch_async` itself, before the
callback is registered (100% reproducible in stub mode, where
`add_host_callback` runs synchronously; see the regression tests
`reaper_finalizes_submission_without_any_poll_call` and
`finalize_enqueued_handle_*` in `../../../vm/src/runtime/offload.rs`).

A `SharedVm` not constructed via `Vm::new` (most unit tests, and any
future embedding that skips it) never populates `self_arc`, so the reaper's
`Weak<SharedVm>` never upgrades — that degrades gracefully to the
pre-existing poll-only behavior rather than panicking or hanging. Each
queued handle carries its own `Weak<SharedVm>` rather than the reaper
thread capturing a single one at spawn time — real-hardware validation
(below) caught that the naive "capture once" version permanently strands
every submission from any `SharedVm` constructed *after* whichever one
happened to start the reaper thread first, the moment that first `SharedVm`
is dropped.

**Hardware-validated (2026-07-12, RTX 2060).** New test
`device_submission_completes_spontaneously_without_any_poll_call`
(`../../../vm/tests/gpu_offload_features.rs`) dispatches through
`dispatch_method_from_native` directly (not `try_dispatch`, which
finalizes synchronously on the calling thread and would trivially "pass"
regardless of the reaper), sleeps with **zero** calls to
`poll_submission_status`/`finalize_submission`/anything else that could
itself drive completion, then reads `StreamSubmission::status` directly —
bypassing every public accessor — to confirm the reaper alone flipped it
to `Completed` and drained the writebacks correctly. Passes.

This run also surfaced a separate, real finding, NOT a defect in the
completion reaper: running all four `#[ignore]`d hardware tests in this
file under `cargo test`'s default parallel test threads intermittently
panics an unrelated background thread with `CUDA_ERROR_NOT_PERMITTED`
inside `cudarc`'s `CudaStream::drop`. Root cause: each test independently
constructs its own `Vm::new()`/`DeviceContext`, but `cudarc::CudaDevice::new(0)`
resolves to the SAME reference-counted CUDA primary context for device 0
across all of them in-process — one test's teardown can release/invalidate
that shared context while another test's still-in-flight async submission
is finalizing on a background thread. Confirmed absent running any single
test alone and running all four with `--test-threads=1`; the doc comment
at the top of `gpu_offload_features.rs` now says so explicitly. This is a
multi-`Vm`-per-process test-harness hazard specific to having several
independent `DeviceContext`s alive for the same device at once — a real
single-VM production process (the normal `cratonvm` deployment shape)
never hits it.

## 4. Small-array thread over-launch (min 2²⁰ threads per launch) — DONE

`dispatch_async` previously computed `work = max(runtime_work,
signature.estimated_work)` where `estimated_work` is a fixed `1 << 20`
placeholder for every counted-loop kernel, so any kernel over a smaller array
still launched 1,048,576 threads. Fixed: `runtime_work` now wins outright when
non-zero; `estimated_work` is only used as the scalar-only sentinel-0 fallback
(`../../../vm/src/runtime/offload.rs`, the `work = if runtime_work > 0 { runtime_work }
else { ... }` branch). A kernel over N elements now launches N threads, not
`max(N, 2^20)`.

## 5. Occupancy-tuned block size is dead code — DONE

`dispatch_async` now uses the occupancy-tuned launch config
(`cuOccupancyMaxPotentialBlockSize` via `DeviceModule::elementwise_for_kernel`
in `../../../cuda-bridge/src/backend_cuda.rs`) instead of the fixed 256-thread
`LaunchConfig::elementwise` block. Rolled out alongside the other dispatch
changes in this update; landed together with the thread-floor fix in item 4
above.

## 6. Analyzer/lowering coverage gaps — DONE

Closed this update:

- **`ldc`/`ldc_w`/`ldc2_w`** — Integer/Float/Long/Double constant-pool loads
  are now resolved against the constant pool and lowered as PTX immediates
  (`../../../jit-cuda/src/analyzer.rs`, `../../../jit-cuda/src/lowering/emit.rs`, "AUDIT C31
  follow-up"). Any int constant outside `sipush` range, or any float/double/long
  literal, is admitted instead of rejecting the whole method.
- **`frem`/`drem`** (IEEE remainder) — real PTX lowering
  (`Emitter::frem_f32`/`drem_f64`), gated behind the existing
  `AdmissionHint::ALLOW_DIV_BY_ZERO` hint (exact only for bounded quotients —
  see `../../gpu/annotations.md`).
- **`lcmp`/`fcmpl`/`fcmpg`/`dcmpl`/`dcmpg`** — bit-exact `setp`/`selp` PTX
  lowering including the NaN-result asymmetry between the `l`/`g` variants.
  The analyzer admits the value-producing comparison opcode itself; a compare
  that immediately *feeds a branch* (the overwhelmingly common javac idiom)
  still rejects at the branch opcode — no false eligibility was introduced.
- **Non-zero-start counted loops** — `i = K; i < bound; i++` with `K >= 0` and
  an `ldc`-sourced `K` is now recognized (`../../../jit-cuda/src/lowering/loop_recog.rs`);
  the emitter folds `K` into the induction variable.
- **`invokestatic` intrinsics** — the PHASE1-GUESS "admit any invokestatic"
  hole is closed. `AdmissionHint::ALLOW_INTRINSIC_CALLS` now resolves the
  constant-pool callee against a curated table
  (`analyzer::resolve_math_intrinsic`) and only admits a call that actually
  lowers; see `../../gpu/annotations.md` for the full supported/excluded list.
- **2-D / rectangular nested loops** (2026-07-12) — `for (i...) for (j...)`
  is now recognized and lowered when the inner loop is the outer loop's
  *entire* body and both loops start at `0`
  (`../../../jit-cuda/src/lowering/loop_recog.rs`'s `detect_nested_loop`/
  `NestedLoop`/`validate_rectangular_nesting`). The SIMT model treats the
  flattened `[0, R*C)` iteration space as the thread-index domain and
  recovers `i = tid / C`, `j = tid % C` on-device
  (`Emitter::emit_nested_loop_guard_and_decompose` in
  `../../../jit-cuda/src/lowering/emit.rs`) — no changes were needed in
  `../../../vm/src/runtime/offload.rs` or `cuda-bridge`, because the existing 1-D
  grid sizing (`runtime_work` = the largest array argument's length) already
  equals `R*C` for any flattened row-major array, and `div.s32`/`rem.s32`
  by the inner bound is provably safe because a thread only reaches the
  division after passing the `tid < R*C` guard. Both loop bounds still have
  to resolve the same way a single loop's bound does — a compile-time
  literal or an `arraylength` of an array parameter hoisted to a local
  (see `EligibleOffsetLoop.java`'s doc comment) — a bound that is itself
  the outer induction variable (a triangular loop) or any other expression
  is rejected at lowering, not silently mis-lowered. Two-level nesting only
  (three-or-more levels, e.g. a 3-D loop, still reject — same "multi-loop or
  non-canonical control flow" reason two *sequential* loops already got).
  Fixture: `../../../test_classes/gpu/EligibleNestedLoop.java`; tests + a real
  `ptxas` round-trip in `../../../jit-cuda/src/lowering.rs`
  (`nested_loop_*`/`ptxas_round_trip_nested_loop`/`triangular_nested_loop_is_rejected`).

**General branches (closed 2026-07-12).** `Emitter::walk_cfg` discovers
forward basic blocks in a counted-loop body, emits `L_body_<pc>` labels, and
lowers `if*`/`if_icmp*`/`ifnull`/`ifnonnull` to `setp` plus predicated PTX
branches. Each edge copies JVM locals and operand-stack values into the target
block's canonical register set, providing explicit phi-style merge semantics
without relying on a linear simulated stack. `goto`/`goto_w` are lowered as
real forward PTX branches. Interior backward edges, branches that leave the
body (`break`/`continue`), and returns in the body remain explicit CPU
fallbacks because they require a different iteration-space model.

`EligibleBranchingLoop.java` and the `branching_loop_*` lowering tests cover
both `if`/`else` local joins and a one-arm branch that falls through to the
canonical loop back-edge. `ptxas_round_trip_branching_loops` validates both
generated PTX shapes when the CUDA integration feature is enabled.

`)F`/`)D` reductions still stay on CPU by design — see item 1.

## 7. No hardware CI — SCAFFOLDING DONE, runner enrollment pending

`../../../.github/workflows/gpu-selfhosted.yml` (weekly, self-hosted GPU runner label)
and `../../../bench-gpu/ci-gate.sh` (runs the `../../../bench-gpu` suite and checks checksums,
same as the manual comparison scripts) now exist and are ready to catch a
regression like the invoke-cache bug or the `atom.global.add`/`ptxas` failure
in item 1 the day it lands. What's still missing is the self-hosted runner
itself — no CUDA box is enrolled against the workflow's runner label yet, so
the job is defined but has nothing to execute on.

## Benchmark snapshot backing this doc

See `../../../bench-gpu/results` (2026-07-11 files) and the README "GPU offload"
section for the CratonVM-vs-TornadoVM-vs-HotSpot numbers, including the
div-chain kernel where the GPU beats even auto-vectorized HotSpot C2 by ~70×.
The 2026-07-11 evening update added `../../../bench-gpu/GpuDotBench.java` (reduction,
item 1), `../../../bench-gpu/GpuLdcBench.java` (ldc constants, item 6), and their
TornadoVM twin `../../../bench-tornado/TornadoDotBench.java`; see the README's "Update
2026-07-11 (evening)" paragraph and `../../gpu/COMPARISON.md` for those
numbers.
Box caveat: measurements taken on a machine with ~25-30% background CPU load
(known infection, see session notes); GPU-side timings are largely unaffected
(GPU idle), CPU baselines are pessimistic by roughly that margin.
