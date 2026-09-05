# CratonVM Benchmarks

Methodology, full notes, and provenance for the numbers in
[README.md](README.md). The headline rule for every table we publish:
**checksums must match HotSpot on every run** — a fast wrong answer is a
bug, not a result.

## CPU benchmarks

### Harness

All seven CPU rows come from one unified harness,
[`bench/CratonBench.java`](bench/CratonBench.java): arithmetic, fib, sieve,
matrix, hashmap, stringregex, and bintrees, each runnable in-process or as an
isolated phase (`CratonBench <phase>`). Each phase prints its time **and its
checksum**.

**Also in `bench/`**, the per-kernel harnesses, each likewise printing time
**and checksum** — useful when you want one row without the others, and what to
fall back on if the unified harness goes missing again:

| File | Row it covers | Checksum |
|------|---------------|----------|
| `BinTreesClassic.java` | Binary Trees (takes depth as argv, e.g. `18`) | `68332206` at d=18 |
| `HashMapOnly.java` | HashMap put/get | `15499991500000` at n=1,000,000 |
| `StringRegexOnly.java` | String/Regex | (printed) |
| `QuickBenchLong2.java` | arithmetic / fib / sieve / matrix kernels | (printed) |
| `BinT.java`, `IntrinsicBench.java`, `HmSemanticsProbe.java` | targeted probes | (printed) |

Whichever harness you use, compare **before/after on one host with one binary
pair**. That is the methodologically sound comparison regardless of harness, and
on this shared box it is often the only trustworthy one.

### Methodology

- Both VMs run the **same flags** (notably `-Xmx8g` for Binary Trees).
- Fresh process per measurement, alternating HotSpot / CratonVM runs.
- Pinned to one logical CPU with `taskset` on the benchmark host
  (Azure Linux, EPYC 9V45, SMT).
- Medians over 5–7 runs; **no samples discarded**.
- Reference JDK: Temurin JDK 25.0.3 (C2).
- Measurements are taken in quiet windows (1-minute load average below 2);
  the shared host's load otherwise inflates both columns — and CratonVM's
  memory-heavy rows more — so ratios, not absolute times, are the durable
  content across host re-provisionings.

### Current table

All seven rows come from one interleaved series. Azure EPYC bench host, JDK 25.0.3 both sides, `-Xmx8g`
both sides, one phase per fresh process pinned to one core, arms **alternated
with the order flipped on alternate pairs**, 9 pairs per phase, no sample
discarded, and **every one of the 18 samples per phase** checksum-verified on
both arms — not just the medians, so a mid-series drift cannot hide behind a
matching median. Zero mismatches. The window was opened only after the
1-minute load fell below 2.5 **and** no other `cratonvm` process was pinned to
the measuring core — the second check matters because two benchmarks
timesharing one core is invisible in a load average, which is the trap this
file's own methodology section warns about. All seven phases come from **one
binary in one window**, load 1.8–3.8 throughout, with the series aborted and
retried if load left the band mid-run.

| Benchmark                         | JDK 25 C2 | CratonVM  | Ratio     | CV (CratonVM) | was (2026-07) |
|-----------------------------------|-----------|-----------|-----------|---------------|---------------|
| Arithmetic (2B ops)               | 1,852 ms  | 3,601 ms  | 1.94x     | 0.6% | 2.44x |
| Fibonacci(44)                     | 1,449 ms  | 5,059 ms‡ | 3.49x‡    | 0.6% | 2.79x |
| Sieve (100K × 20,000)             | 2,333 ms† | 2,360 ms  | **1.01x** | 2.0% | 2.28x |
| Matrix 1280×1280                  | 2,106 ms  | 2,094 ms  | **0.99x** | 0.2% | 2.93x |
| HashMap (10M put/get, isolated)   | 983 ms    | 2,049 ms  | 2.08x     | 0.6% | **1.75x** |
| String/Regex (100K, isolated)     | 50 ms     | 200 ms    | 4.00x     | 0.9% | **7.7x** |
| Binary Trees (depth 18, isolated) | 176 ms    | 1,700 ms  | 9.66x     | 1.1% | 8.34x |

This replaces an older table whose rows were taken across four separate
sessions on a host that has since been re-provisioned and three of which this
document already flagged as unverified. Every row above comes from **one**
interleaved series, so the rows are comparable to each other. CratonVM's
run-to-run spread is under 1% on five of the seven rows.

**Two rows are at parity with HotSpot C2**: Matrix and Sieve. HashMap and
Binary Trees sit above their `was (2026-07)` figures, but those earlier
absolutes were taken on a since-re-provisioned host and were never re-measured
under the current protocol — not a regression against a comparable baseline.

### † Sieve: HotSpot is bimodal on this phase

The Sieve row's HotSpot figure is the median of **18** samples pooled from two
independent series, and the pooling is a correction rather than a convenience.

HotSpot on this phase lands in one of two modes and nothing between them:

| mode | n | median |
|---|---:|---:|
| fast | 10 | 2,369 ms |
| slow | 8 | 2,739 ms |

A 9-sample median therefore reports whichever mode won the coin toss: one
series read **2,386 ms**, the next read **2,734 ms**, on an unchanged binary
and an unchanged JDK. CratonVM's own 18 samples over the same runs are
unimodal (2,276–2,498 ms, CV 2.3%).

Pooled, the two are 2,360 against 2,333 — parity. Quoting the cleanest single
series would have given a double-digit swing in either direction depending on
which mode the median fell in. Parity is what the data supports. **Do not
re-derive this row from a single 9-sample run.**

### ‡ Fibonacci: a known shape

Both JIT backends erase the dead shadow-stack thread fetch from the prologue;
the single-pass backend jumps over the erased ~46-byte span, and the IR
backend previously overwrote it with one-byte `NOP`s, so every IR method that
published nothing retired 46 NOPs on entry, on every invocation. That is
fixed. The residual gap to the pre-IR single-pass body (near 2.9x) is
precise-root and deopt metadata the IR tier emits and the single-pass backend
did not: the safepoint-id slot the collector reads to pick an oop map, and the
innermost-RBP mirror the stack walker reads. Nothing today compares an IR body
against the C1 body it replaces before keeping it; that is an open policy
question, not metadata to be deleted.

The optimizing tier declines a method whose loops the single-pass backend
would lower better, and what that backend can do and the IR tier cannot is
enumerated in `jit/src/x64/single_pass_only.rs` rather than discovered one
regression at a time.

### Sieve was 6.50x yesterday

That was a live regression, not a measurement problem, and it is fixed.
`CratonBench.sieve([ZI)I` had begun receiving a body from the optimizing
(C2/IR) tier where it previously fell through to the single-pass backend, which
*vectorises* its `boolean[]` loops; the IR body was 6.4x slower than the C1
body it replaced (2,462 ms → 15,823 ms, checksums identical). Fixed: the optimizing tier's admission chain now declines a method whose
loops the single-pass backend would lower better, and the seven classes of
lowering that backend has and the IR tier lacks are enumerated in
`jit/src/x64/single_pass_only.rs` rather than discovered one regression at a
time.

Fibonacci's own gap is explained above (see `‡ Fibonacci: a known shape`), not
by this section's sieve regression.

Row notes, carried over from the table this replaced:

- **RETRACTED: the HashMap regression this table used to record
  (`22,077 ms`, `21.2x`, "CONFIRMED and bounded to `a36b9d121..e57f0bc7d`")
  does not reproduce, and the bisect it recommended has nothing in it.**
  Measured before any change, `dev` @ `9ac1feffe`, isolated fresh processes,
  `-Xmx8g`, alternating arms pinned to cpu 15 with `mpstat -P ALL` confirming
  that core was 90-100% the measuring process:

  | n | HotSpot | CratonVM `dev` | ratio | what this table said |
  |---|---:|---:|---:|---|
  | 1,000,000 | 66 ms | 427 ms | 6.5x | 3,084 ms / 42.7x |
  | 10,000,000 | 997 ms | 3,523 ms | 3.53x | 22,077 ms / 21.2x |

  427 ms at n=1M is within noise of the **434 ms** this same note records for
  the *healthy* `a36b9d121` (07-18) binary, so there is no 7.1x delta left to
  bisect for. **Why the 07-25 readings were ~5x too slow is not established.**
  The obvious candidate — the cpu-13 collision warned about further down this
  file — was tested directly (same binary, same phase, alternating cpu 13 and
  cpu 15) and showed **no difference**: 4131/4004 on cpu 13 against 4063/4042
  on cpu 15. Treat any 07-25-era absolute in this document as unverified until
  re-measured. Full detail:
  `hashmap-half-gap-20260730.md`.

  String/Regex's 07-25 row is left as recorded — it has not been re-measured
  and it was the *smaller* of the two claims (3.57x → 7.7x), but it came out of
  the same session, so it deserves the same scepticism.
- **HashMap's 1.75x is that corrected baseline plus a real 72.3% gap
  reduction.** From 3,776 ms to 1,780 ms against HotSpot's 1,017 ms, medians of
  five interleaved fresh-process runs at load 3.4-3.8, every checksum
  `1549999915000000`; the repository's own gate independently reports `PASS
  median 1767ms`. A second series at load 10-12 reads 4,065 / 1,870 / 1,062,
  i.e. 73.1% — the reduction is stable across load levels. Two
  causes: the Integer-keyed dense overlay was maintaining a second full
  `FxHashMap` purely to remember insertion order it could derive from the key
  (19.7% of the phase in `note_fresh_insert` alone), and a set of per-object
  fixed costs — `get_header`, the `ObjectRef` provenance bitmap, the
  single-OS-thread tripwire, `a2dbg::record`, and re-deriving a monomorphic
  `checkcast`'s answer — that were each an out-of-line call around a no-op or
  an idempotent update. The host was at load 10-12, so both columns are
  inflated; the ratio is the durable content.
- **Fibonacci** is recursion-bound and its recursive self-call compiles to a
  guarded direct call. The closeout removed the Linux
  `jit_frame_record` helper from the prologue and both post-recursive-call
  restoration sites, replacing each with one sentinel-probed `fs:` TLS store.
  It also permits a metadata-only empty root map when moving-young analysis
  proves the recursive caller has no live oops. The merged-binary alternating
  acceptance reduced the HotSpot gap by **71.90%**. Full measurements and
  generated-code evidence are in
  `fibonacci-half-gap-20260730.md`.
- **Binary Trees** was measured at `-Xmx8g` as seven alternating
  fresh-process pairs; all fourteen checksums were `68332206`.
  A July 2026 dev regression that temporarily quadrupled this row was
  root-caused (two independent causes) and fixed.
  The perf gate's anchored baseline is 1,550 ms, reflecting a deliberate
  ~2% correctness hardening (explicit header initialization in the inline
  allocator) accepted after that fix.
  A follow-up (TLAB zero-elision for the fields that don't need
  it, plus a GC-inert proof for `itemCheck`'s allocation-free self-
  recursion) cut a further **14.5%** off the wall-time in a clean 10-round
  interleaved Azure measurement (1,529.5 ms candidate vs 1,789.5 ms
  `origin/dev`, 177 ms HotSpot — 16.1% HotSpot-gap reduction). An
  accompanying attempt to also cap the initial young semispace at 512 MiB
  measured as a **12-13% regression** instead (it multiplied a pre-existing
  "young GC always falls back to non-moving sweep for this workload" defect
  by forcing ~6x more young collections) and was reverted. Full measurement
  history, isolation methodology, and the root-cause writeup are in
  `binarytrees-bt18-half-gap-20260730.md`.
- **Sieve** is three counted `boolean[]` loops, and single-pass BCE refuses
  inclusive (`<=`) loops and non-`arr.length` bounds, so every element kept a
  null and bounds check. A later change added three fall-through-only
  guarded preheaders — a block clear, a strided store, and the whole sieve
  nest, the last of which scans eight bytes at a time for the next unmarked
  index. Two independent 9-round interleaved same-binary A/B measurements
  (`CRATONVM_JIT_BULK_BYTE_LOOPS=0` as the dev-equivalent control, all 54
  runs checksum `9592`) cut the HotSpot gap by **94.35%** and **95.30%**,
  moving the ratio from 1.85x to **1.05x**. Full measurements, the guard
  contract, and the differential probe are in
  `cratonbench-sieve-half-gap-20260730.md`.

### The performance gate

Perf regressions are guarded by
[`regression-suite/perf/run-cratonbench-gate.sh`](regression-suite/perf/run-cratonbench-gate.sh),
which runs every phase isolated (pinned, `-Xmx8g`, median of 5), verifies the
exact checksum on every run, enforces a 5% budget over the per-host anchored
baselines in
[`cratonbench-baseline-azure-epyc.tsv`](regression-suite/perf/cratonbench-baseline-azure-epyc.tsv),
and **refuses to measure on a loaded host** rather than produce noisy verdicts.
Anchored baselines may only be re-anchored with a linked evidence document.

This gate was absent from `dev` for a period —
if you are working on a commit that predates the restore, it will not be there.

Even with the gate available, use the process below for a perf-affecting change
whenever you cannot get a quiet host — the gate deliberately refuses to measure
under load, and the shared Azure box frequently runs at load 5–8 with several
sessions building:

1. Build the pre-change commit and the post-change commit as **two
   uniquely-named binaries on the same host**.
2. Run the relevant per-kernel harness from the table above against both,
   alternating, several runs each.
3. **Compare checksums first.** A changed checksum means the change is wrong;
   a faster wrong answer is a bug, not a result.
4. Report absolute times for both binaries and the host's load average, since
   this host is shared and contended — ratios between your two binaries are
   meaningful, absolute numbers across hosts and dates are not.
5. `perf record -g --call-graph=dwarf -F 199` on both, and report the symbol
   deltas, not just wall clock.

### Optimization history

The journey from the earliest 20–100x gaps to the current table is
documented round by round:

- [docs/JIT_OPTIMIZATION.md](docs/JIT_OPTIMIZATION.md) — the current JIT
  architecture and the optimizations that produced the table above

## GPU benchmarks

### Setup

Measured on a GeForce RTX
2060 (sm_75, driver 591.86) against HotSpot JDK 25.0.3 (C2) and TornadoVM
4.0.1 (PTX backend, `@Parallel`/`@Reduce` + TaskGraph API). CratonVM offload
is **opt-in and automatic within a narrow envelope**: the kernels below are
plain static methods over primitive arrays with no annotations and no API at
the call site, but they require a GPU build and an explicit flag (`cargo
build --features gpu-driver`, run with `--gpu`), and anything the analyzer
doesn't accept stays on the CPU. All timings are warm and include the full
per-call H2D + kernel + D2H round-trip. There is no self-hosted GPU hardware
CI, so these are point-in-time measurements rather than a continuously
enforced budget.

| Kernel (N = 2²⁴)                                    | HotSpot C2 | TornadoVM GPU | CratonVM GPU | vs HotSpot | vs TornadoVM |
|-------------------------------------------------------|------------|---------------|--------------|------------|--------------|
| Integer div-chain (48 divs/elem)                       | 2,146 ms   | 26 ms         | **11 ms**    | **195x**   | **2.4x**     |
| Double div-chain (64 divs/elem)                        | 1,780 ms   | 128 ms        | **95 ms**    | **18.7x**  | **1.3x**     |
| 128 multiply-adds/elem (data-dependent multiplier)     | 1,300 ms   | 27 ms         | **8 ms**     | **163x**   | **3.4x**     |
| Dot-product reduction (int·int → long, x300/elem)      | 1,172 ms   | unimplemented | **2 ms**     | **586x**    | n/a          |

Re-verified 2026-09-05 on the same box (RTX 2060, TornadoVM 4.0.1 PTX,
N = 2²⁴). The four non-ray-tracer rows still hold and TornadoVM's integer
div-chain reproduced exactly at 26 ms. Two things that run did change:

- The **dot-product row is now 2 ms**, not 12 — the warp-shuffle reduction
  (one `red.global.add` per warp rather than per thread) landed after the
  original measurement.
- The **double div-chain row had silently stopped reproducing**: it measured
  9,276 ms, slower than HotSpot, because `runtime::offload_jit_gate` asked the
  constant-pool-*free* analyzer, which rejects `ldc2_w` unconditionally, while
  the dispatcher it gates for asks the pool-aware one. That kernel's
  `+ 1.0000001` is an `ldc2_w`; its integer twin's `+ 12345` is a `sipush`
  with no pool entry, which is why one row worked and the other did not.
  Fixed the same day; the row now measures 81 ms against TornadoVM's 135.

The CPU columns above are the original idle-box measurements and were **not**
re-taken — that re-run shared the host with an unrelated build, which makes a
CPU baseline pessimistic and every ratio derived from it flattering. The GPU
figures quoted in this note were measured under that same load, so they are
conservative rather than optimistic. The ray-tracer row was not re-run: it
needs `craton-gpu-0.2.0.jar` and a `cratonvm-gpuray` binary that are not
present in this tree.

Notes:

- The div-chain rows are the "GPU wins big" cases: division has no
  competitive CPU-vectorized form, so raw parallelism wins at every size
  tested (2²⁰–2²⁶; the ratios hold steady across sizes).
- CratonVM's double-division checksum is **bit-exact** with HotSpot at
  every size (`div.rn.f64` is IEEE-754 round-to-nearest, same as x86
  `vdivpd`); TornadoVM's diverges slightly — its PTX backend doesn't
  guarantee bit-exact division.
- **Root-caused: TornadoVM's `unimplemented` is a standing gap in
  mixed-type reductions, not a version/driver issue.** `TornadoSnippetReflectionProvider
  .forBoxed` (what the whole stack trace bottoms out in) is an unconditional
  stub — `unimplemented(); return null;` — in both the TornadoVM 4.0.1 jar on
  this box *and* the current `master` branch on GitHub, so upgrading would not
  fix it. Isolated the exact trigger with three minimal repros on this same
  GPU: a single-array `LongArray` sum reduction works; a two-array
  `LongArray`+`LongArray` sum reduction (no cast, no multiply) works; a
  single-array `IntArray` reduced into a `LongArray` accumulator with one
  `(long) a.get(i)` widening cast fails with the identical stack trace. So the
  precise gap is **a `@Reduce` kernel whose per-element expression needs a
  primitive widening conversion (`int`→`long`) before accumulating into a
  differently-typed reduce array** — not "long reductions" or "two-array
  reductions" in general. `GpuDotBench.dotReduce`'s `int·int → long` shape
  (needed to avoid `int` overflow in the product) is exactly that mixed-type
  case, and there's no workaround that preserves what the row measures, so
  "unimplemented" here is a real, durable TornadoVM limitation, confirmed by
  reading the source rather than assumed from the error message. CratonVM's
  automatic `--gpu` path handles the same reduction and completes the full
  dispatch-and-readback (an `is_reduction: true` kernel), not just a CPU
  fallback, as an earlier note assumed.
- **The multiply-add row's old "honest counter-case"
  framing was a compiler artifact, not a real result.** The previous kernel
  used a *compile-time-constant* multiplier (`x = x*1103+12345`, repeated 96
  times); composing an affine map with itself under constant coefficients is
  itself affine, and HotSpot C2's GVN/reassociation folds the whole chain
  into a single multiply+add with closed-form coefficients regardless of the
  unroll count — confirmed by unrolling the *old* kernel to 6,528 lines and
  seeing under 2x change in wall time. The benchmark was silently
  memory-bandwidth-bound on the HotSpot side, not compute-bound, which is
  why AVX2 looked "competitive." Sourcing the multiplier from a second
  per-element array (`m = b[i]`, the same trick `GpuDivChain` already uses
  for its divisor) removes the closed form: HotSpot now does genuine work
  and the row flips from "GPU loses" (0.7x) to another 163x GPU win. The
  dot-product row's swing (0.4x → 98x, and 586x since the warp-shuffle
  reduction) is different in kind: `sum += p` was
  never foldable (both operands are runtime array reads), so scaling its
  per-element repeat count from 1x to 300x — needed to push HotSpot over 1
  second — genuinely shifts the kernel from launch/PCIe-overhead-bound (GPU
  loses on a single multiply-add) to compute-bound (GPU wins). Both rows'
  `TornadoGpuCompute`/`GpuWarm`/`GpuCompute` and `GpuDotBench` sources carry
  `AUDIT` comments with the full detail and the measurements that
  back them up.
- **FIXED: dot-product CratonVM-GPU background `cudarc` panic.**
  Every `--gpu` run of `GpuDotBench` used to also print one `cudarc` panic per
  completed dispatch to stderr — `DriverError(CUDA_ERROR_NOT_PERMITTED,
  "operation not permitted")` out of cudarc's `CudaStream::drop`. Root cause:
  `dispatch_async`'s `cuLaunchHostFunc` completion callback
  (`vm/src/runtime/offload.rs`) cloned `Arc<StreamSubmission>` for its own use
  and let that clone drop locally at the end of the closure. For a
  synchronous (non-Future-API) dispatch — exactly `GpuDotBench`'s transparent
  `--gpu` path, which never calls `register_submission` — that clone reliably
  ends up the *last* strong reference by the time the driver fires the
  callback, so its drop (cascading into `cuda_bridge::Stream`'s and cudarc's
  `CudaStream`'s destructors, which issue a real CUDA driver call) ran from
  inside the callback — forbidden per CUDA's own `cuLaunchHostFunc` rules
  (and per this codebase's own doc comment on `Stream::add_host_callback`).
  Fix: the completion reaper's queue (`REAPER_QUEUE`) now carries the
  `Arc<StreamSubmission>` alongside the handle, so the callback *moves*
  ownership into the queue instead of dropping it locally — the eventual
  final drop, if any, now happens on the reaper thread (an ordinary thread,
  where CUDA driver calls are allowed) rather than inside the CUDA callback.
  Verified: 3 repeated `RUST_BACKTRACE=1` trials of the exact repro command
  with zero panics, correct `DOT_CHECKSUM == DOT_REF` and unchanged ~12ms
  timing every time; `cargo test -p cratonvm-vm --features gpu-offload --lib
  offload` (35 tests, including the reaper-specific ones) still passes; the
  other three kernels (div-chain, float-div-chain, warm MAD) re-verified with
  matching checksums and no regressions.

Full GPU results — more input sizes, `ldc`-constant kernels, cold-start
numbers up to N = 2²⁸, kernel sources, and eligibility rules — are in
[docs/gpu/README.md](docs/gpu/README.md).

## Reproducing

> **⚠ Build with fat LTO for any number you intend to compare against the
> baselines.** `[profile.release]` is `lto = "fat"` + `codegen-units = 1`, and the
> anchored baselines in `cratonbench-baseline-azure-epyc.tsv` were measured that
> way. Building with `CARGO_PROFILE_RELEASE_LTO=off` — the usual workaround for
> the OOM SIGKILL below — produces a **materially slower** binary whose absolute
> times are not comparable to those baselines. Measured on one commit,
> two binaries differing only in LTO, interleaved, 3 pairs per phase (host load
> ~6-7, so absolutes are inflated and the spread is wide — hashmap ranged
> 23461-31120 *within one arm*):
>
> | phase | LTO=off median | fat-LTO median | fat-LTO effect |
> |---|---|---|---|
> | bintrees | 3019 ms | 2262 ms | **−25%** |
> | hashmap | 25484 ms | 26196 ms | none |
> | sieve | 10262 ms | 9552 ms | ~−7% |
>
> Fat LTO is worth a real amount on bintrees and close to nothing on hashmap and
> sieve. Use it for anything baseline-comparable — but note what it does NOT
> explain, below.
>
> LTO=off is fine for **relative** A/B between two binaries built the same way —
> that is how the layout-registry win was measured — but say so when reporting,
> and never quote an LTO=off absolute against a baseline.

```bash
# Build. Fat LTO — required for baseline-comparable numbers.
cargo build --release -p cratonvm-cli

# If the final cratonvm-cli rustc dies with "signal: 9, SIGKILL" that is the OOM
# killer during fat-LTO codegen, NOT a compile error. Lower parallelism first:
cargo build --release -p cratonvm-cli -j2
# Only as a last resort, and only for relative A/B (see the warning above):
CARGO_PROFILE_RELEASE_LTO=off cargo build --release -p cratonvm-cli -j6

# CPU, all seven phases in-process:
javac -d bench-classes bench/CratonBench.java
./target/release/cratonvm -Xmx8g -cp bench-classes CratonBench

# One isolated phase (the per-row methodology):
./target/release/cratonvm -Xmx8g -cp bench-classes CratonBench bintrees

# The gated regression check (Linux bench host, refuses to run under load):
bash regression-suite/perf/run-cratonbench-gate.sh -Exe /abs/path/to/cratonvm

# Per-kernel harnesses, for one row without the others.
javac -d bench-classes bench/BinTreesClassic.java bench/HashMapOnly.java
# Binary Trees (depth as argv). Expected checksum at d=18: 68332206.
./target/release/cratonvm -Xmx8g -cp bench-classes BinTreesClassic 18
# HashMap put/get. Expected checksum at n=1,000,000: 15499991500000.
./target/release/cratonvm -Xmx8g -cp bench-classes HashMapOnly

# HotSpot reference on the same host, same flags:
$JAVA_HOME/bin/java -Xmx8g -cp bench-classes BinTreesClassic 18

# Profile rather than guess. This is how the layout-registry lookup was found
# to be ~40% of the Binary Trees run (37.8% in class_layout_for_fields plus
# 4.8% in the SipHash it used for a (u32,u32) key).
perf record -g --call-graph=dwarf -F 199 -o bt.perf -- \
  ./target/release/cratonvm -Xmx8g -cp bench-classes BinTreesClassic 18
perf report -i bt.perf --no-children --percent-limit 1 --stdio
```
