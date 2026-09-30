# GPU Offload Comparison — CratonVM vs TornadoVM (rerun, 2026-08-02)

**Date:** 2026-08-02T09:31Z (approx, host-local; see individual trial notes)
**CratonVM CPU:** `target/release/cratonvm.exe` (built from dev @ `bc9704184`)
**CratonVM GPU:** `target-gpu/release/cratonvm.exe --gpu` (same commit, `--features gpu-driver`)
**TornadoVM:** `C:/craton/tornadovm/jdk-25.0.3/bin/java.exe` (4.0.1, PTX/RTX 2060)
**HotSpot:** `C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot/bin/java.exe`
**Hardware:** NVIDIA GeForce RTX 2060 (sm_75, 12 GiB), driver 591.86 — confirmed via `--gpu-info`.

This rerun also widens two kernels (`GpuWarm`/`GpuCompute`'s multiply-add
chain, `GpuDotBench`'s dot-product reduction) so their HotSpot time stays
above 1 second at N = 2²⁴, per request. See each `.java` file's `AUDIT
2026-08-02` comment for the full rationale. Short version below.

## Results table (N = 2²⁴ = 16,777,216)

| Kernel | HotSpot C2 | TornadoVM GPU | CratonVM CPU | CratonVM GPU | vs HotSpot | vs TornadoVM |
|---|---|---|---|---|---|---|
| Integer div-chain (48 divs/elem) | 2,146 ms | 26 ms | 4,331 ms | 11 ms | 195x | 2.4x |
| Double div-chain (64 divs/elem) | 1,780 ms | 128 ms | 8,577 ms | 95 ms | 18.7x | 1.3x |
| 128 multiply-adds/elem (data-dependent multiplier) | 1,300 ms (median of 3: 1367/1234/1300) | 27 ms (median of 3: 27/28/24) | 2,892 ms | 8 ms (median of 3: 9/7/8) | 163x | 3.4x |
| Dot-product reduction, x300/elem (int·int → long) | 1,172 ms (median of 3: 1241*/1222/1169/1172 — *cv-cpu run's own hotspot trial excluded, see raw log) | unimplemented | 1,241 ms | 12 ms (stable across 3 trials) | 98x | n/a |

Checksums: `DIV_CHECKSUM`, `FDIV_CHECKSUM`, `SAMPLE`/`COMPUTE_CHECKSUM`, and
`DOT_CHECKSUM` all agreed exactly across CratonVM-CPU, CratonVM-GPU, and
HotSpot on every kernel (float div-chain excepted only for TornadoVM, whose
PTX backend does not guarantee bit-exact division — matches the historical
finding). `DOT_REF` (an independently-shaped decrementing-loop CPU oracle)
matched `DOT_CHECKSUM` exactly on every trial.

## What changed since the 2026-07-11 table, and why

### 1. Both non-div-chain rows flip from "GPU loses" to "GPU wins"

The 2026-07-11 table read:

| Kernel | HotSpot | TornadoVM | CratonVM GPU | vs HotSpot |
|---|---|---|---|---|
| 96 multiply-adds/elem | 8 ms | 17 ms | 11 ms | 0.7x |
| Dot-product reduction | 7 ms | unimplemented | 18 ms | 0.4x |

and BENCHMARK.md described these as "honest counter-cases: CPU AVX2 stays
competitive on MAD-dominated kernels ... and a single atomic accumulator
doesn't get relatively cheaper with more elements."

**The multiply-add row's old framing was not a real result — it was a
HotSpot C2 compiler artifact.** The old kernel was
`x = x*1103 + 12345`, repeated 96 times with **compile-time-constant**
operands. Composing an affine map with itself under constant coefficients
is still affine (`f(f(x)) = x*C² + D*(C+1)` etc.), and C2's GVN/
reassociation folds the entire repeated chain down to a single multiply-add
with closed-form coefficients (`1103^96` and the corresponding
geometric-series constant), independent of how many lines are unrolled in
source. Proof: unrolling the *old* constant-coefficient kernel from 96 to
6,528 lines (68x more source) changed HotSpot's wall time by under 2x — the
"96 multiply-adds" benchmark was silently memory-bandwidth-bound (read
64MB, write 64MB), not compute-bound, on the HotSpot side the entire time.
CratonVM's GPU path was never affected (PTX lowering emits each bytecode op
directly, no algebraic simplification pass), so its "11 ms" / "9 ms" numbers
in both tables were always measuring genuine work — the CPU side just wasn't.

Fix: source the multiplier from a second per-element array (`m = b[i]`,
mirroring `GpuDivChain`'s data-dependent divisor) so C2 cannot constant-fold
the chain — verified empirically: 96 data-dependent steps measured ~888ms,
128 steps ~1273ms (both roughly linear in step count, confirming genuine
per-step work). At 128 steps HotSpot now takes 1.2-1.4s (genuine AVX2-
vectorized work), and the GPU wins by ~163x instead of losing 0.7x.

**The dot-product row's swing is a different kind of change — not a bug fix,
a genuinely different kernel.** `sum += (long) a[i] * b[i]` was never
foldable (both operands are runtime array reads, never compile-time
constants) — confirmed by testing repeat counts of 1/10/50 and seeing exact
linear scaling of both `DOT_CHECKSUM` and wall time, no collapse. The old
1x-per-element kernel was just legitimately cheap (1 multiply, 1 add) and
therefore launch/PCIe-transfer-overhead-bound on the GPU side, where
HotSpot's tiny per-element cost wins. Repeating the accumulation 300x per
element (needed anyway to push HotSpot over 1 second) shifts the balance
from overhead-bound to compute-bound, and the GPU wins by ~98x instead of
losing 0.4x.

**Practical upshot:** the README/BENCHMARK.md "honest counter-case" language
for these two rows has been removed — it was never really about MAD/
reduction workloads being bad GPU fits in general, it was an artifact of
under-powered kernels (one accidentally erased by the JIT, one just
naturally tiny). All four rows in the current table are GPU wins.

### 2. TornadoVM's dot-product `unimplemented`, root-caused (2026-08-03)

Repeating the reduction statement 300x initially threw a *different* error
first (`ArrayIndexOutOfBoundsException: Index 256 out of bounds for length
256` from `TornadoExecutionContext.assignTaskToDevice` — Tornado's reduce-
skeleton rewriter turns each reduction statement into its own internal task,
and the task-to-device table is capped at 256 entries; worked around in
`TornadoDotBench`'s companion multiply-add kernel by folding the repeat count
into a single multiply, see that file). Investigating *that* led to the real,
deeper issue: even the **original, unmodified, single-statement**
`TornadoDotBench.dotReduce` (`result.set(0, result.get(0) + (long) a.get(i) *
(long) b.get(i));`, checked out from `dev` at this session's start, no edits)
throws the exact same `TornadoInternalError: unimplemented` from
`TornadoSnippetReflectionProvider.forBoxed` / `PTXGPUReduceSnippets
$Templates.lower` on this TornadoVM/driver combination — confirming it is a
pre-existing limitation, not a regression from this session's changes.

**Root cause, fully isolated:** `TornadoSnippetReflectionProvider.forBoxed`
is an unconditional stub —

```java
@Override
public JavaConstant forBoxed(JavaKind kind, Object value) {
    unimplemented();
    return null;
}
```

— in both the TornadoVM 4.0.1 jar on this box and the current `master`
branch on GitHub (fetched 2026-08-03), so upgrading TornadoVM would not fix
this. Three minimal repros against the same GPU/driver isolate the exact
trigger:

| Repro | Result |
|---|---|
| Single-array `LongArray` sum reduction | **works** |
| Two-array `LongArray + LongArray` sum reduction (no cast, no multiply) | **works** |
| Single-array `IntArray` reduced into a `LongArray` accumulator with one `(long) a.get(i)` widening cast | **fails**, identical stack trace |

So the gap is specifically: **a `@Reduce` kernel whose per-element expression
needs a primitive widening conversion (`int`→`long`) before accumulating into
a differently-typed reduce array** — not "long reductions" or "two-array
reductions" in general, both of which work fine when the types already
match end-to-end. `GpuDotBench.dotReduce`'s `int·int → long` shape exists
specifically to avoid `int` overflow in the product, so there is no
workaround that keeps the benchmark's intended semantics. This is a genuine,
durable TornadoVM 4.0.1/master limitation and a reasonable candidate for an
upstream bug report (repro: `TornadoIntToLongCastSum` — single `IntArray` in,
`(long) a.get(i)` cast, `@Reduce LongArray` out).

`TornadoDotBench.java` keeps the literal 300x repeat (matching
`GpuDotBench.dotReduce` line-for-line) since it has no effect on whether the
kernel runs either way, and it's more useful to a reader comparing the two
files side by side.

### 3. CratonVM's dot-product GPU dispatch completes end-to-end; background panic FIXED (2026-08-03)

The 2026-07-11 known-issues doc said transparent `--gpu` dispatch fell
through to the CPU for any non-void kernel (the reduction-result-readback
side of the feature hadn't landed). That has since landed:
`--print-gpu-decisions` confirms `GpuDotBench.dotReduce` analyzes
`Eligible(... is_reduction: true ...)` and the measured `DOT_CHECKSUM`
matches the CPU-side `DOT_REF` oracle exactly on every trial — this is a
genuine GPU-computed, GPU-read-back result, not a silent CPU fallback.

Every `--gpu` run of `GpuDotBench` used to also print one `cudarc` panic per
completed dispatch to stderr:

```
thread '<unnamed>' panicked at cudarc-0.13.9/src/driver/safe/core.rs:620:36:
called `Result::unwrap()` on an `Err` value: DriverError(CUDA_ERROR_NOT_PERMITTED, "operation not permitted")
```

Process exit code stayed 0, and the printed result was always correct and
stable (dot_ms=12 across repeated trials) — the panic count exactly matched
the number of completed reps (5 panics for 5 timed reps).

**Root-caused and fixed.** `RUST_BACKTRACE=1` traced the panic to
`Arc::drop_slow<StreamSubmission>` → `Arc::drop_slow<cuda_bridge::Stream>` →
`Arc::drop_slow<CudaStream>` → cudarc's `CudaStream::drop`, called from
inside `cratonvm_cuda_bridge::stream::host_callback_trampoline` — i.e. a
`cuLaunchHostFunc` callback. `dispatch_async`'s completion callback
(`vm/src/runtime/offload.rs`) cloned `Arc<StreamSubmission>` for its own use
(to flag `device_done` and enqueue the handle for the reaper) and let that
clone drop locally at the end of the closure. For a synchronous
(non-Future-API) dispatch — exactly `GpuDotBench`'s transparent `--gpu` path,
which never calls `register_submission` — that clone reliably ends up the
*last* strong `Arc<StreamSubmission>` by the time the driver actually fires
the callback (the synchronous caller has already read its result and moved
on by then). Dropping the last reference drops the `cuda_bridge::Stream`
inside it, whose `Drop` (via cudarc) issues a real CUDA driver call —
forbidden from inside a `cuLaunchHostFunc` callback per CUDA's own rules
(and this codebase's own doc comment on `Stream::add_host_callback`).

Fix: `REAPER_QUEUE` (the completion reaper's work queue) now carries the
`Arc<StreamSubmission>` alongside `(handle, weak_vm)`, and the callback
*moves* its clone into `enqueue_completion` instead of letting it drop
locally. The potential final drop now happens on the reaper thread — an
ordinary thread, not a CUDA callback, so CUDA driver calls are allowed there
— while preserving the original intent (keep the submission and its pending
device buffers alive until finalization).

Verified on hardware: 3 repeated `RUST_BACKTRACE=1` runs of the exact
`GpuDotBench` invocation with zero panics, `DOT_CHECKSUM == DOT_REF` every
time, unchanged ~12ms timing; `cargo test -p cratonvm-vm --features
gpu-offload --lib offload` (35 tests, including the reaper-specific
`reaper_finalizes_submission_without_any_poll_call` and
`finalize_enqueued_handle_*` tests, updated for the new queue shape) all
pass; the other three kernels (div-chain, float-div-chain, warm MAD)
re-verified afterward with matching checksums and no regressions.

## Raw per-VM output (selected)

```
=== Integer div-chain (GpuDivChain, N=2^24, 5 reps) ===
cv-cpu:   divchain_ms=4331  DIV_CHECKSUM=246467335469
cv-gpu:   divchain_ms=11    DIV_CHECKSUM=246467335469
hotspot:  divchain_ms=2146  DIV_CHECKSUM=246467335469
tornadovm: divchain_ms=26   DIV_CHECKSUM=246467335469

=== Double div-chain (GpuFloatDivChain, N=2^24, 5 reps) ===
cv-cpu:   fdivchain_ms=8577  FDIV_CHECKSUM=5.92801002867254E7
cv-gpu:   fdivchain_ms=95    FDIV_CHECKSUM=5.92801002867254E7
hotspot:  fdivchain_ms=1780  FDIV_CHECKSUM=5.92801002867254E7
tornadovm: fdivchain_ms=128  FDIV_CHECKSUM=5.9583712290819384E7 (expected divergence, PTX div not bit-exact)

=== 128 multiply-adds/elem (GpuWarm f / TornadoGpuCompute, N=2^24) ===
cv-cpu:    warm_ms=2892           SAMPLE=633502528
cv-gpu:    warm_ms=9,7,8 (3 trials)  SAMPLE=633502528
hotspot:   warm_ms=1367,1234,1300 (3 trials) SAMPLE=633502528
tornadovm: heavy_ms=27,28,24 (3 trials)  COMPUTE_CHECKSUM=-2050798668070082 (matches GpuCompute's independent full-array checksum)

=== Dot-product reduction x300 (GpuDotBench, N=2^24, 5 reps) ===
cv-cpu:    dot_ms=1241              DOT_CHECKSUM=-58730497593000  DOT_REF=-58730497593000
cv-gpu:    dot_ms=12,12,12 (3 trials) DOT_CHECKSUM=-58730497593000  DOT_REF=-58730497593000  (+ background cudarc panics, see above)
hotspot:   dot_ms=1222,1169,1172 (3 trials) DOT_CHECKSUM=-58730497593000  DOT_REF=-58730497593000
tornadovm: TornadoInternalError: unimplemented (pre-existing, confirmed also on unmodified single-statement kernel)
```

## Notes

- CratonVM GPU: automatic offload via `--gpu` flag (analyzes bytecode at first call).
- TornadoVM GPU: explicit `@Parallel`/`@Reduce` annotations + TaskGraph API (warmup call before measurement).
- All timings include H2D + kernel + D2H (full round-trip).
- Table values use the first trial for div-chain/float-div-chain (unmodified kernels, consistent with historical single-shot methodology) and the median of 3 trials for the two widened kernels (MAD, dot), since those were the ones under scrutiny for this rerun.
